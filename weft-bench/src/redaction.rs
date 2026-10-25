//! Secret-safe capture for the run manifest (1.0 plan EVI-3).
//!
//! A run manifest is written to be published: it is the artifact a benchmark claim points at, and
//! the nightly track uploads it as a CI artifact. So it records things that can carry a credential —
//! the command line it was invoked with, the working directory, a GPU or engine description quoted
//! from a driver — and a benchmark harness has to be the place that decides those strings are safe
//! before they reach a public file, not the place that discovers otherwise later.
//!
//! Two properties, in order of importance:
//!
//! * **Nothing leaves that looks like a credential.** Known key names, known token shapes, URL
//!   user-info and private-key blocks are replaced with `[redacted]` whatever the surrounding text.
//! * **Nothing ordinary is mangled.** A CPU model, a file system name, a driver version and a mount
//!   point outside the home directory pass through unchanged: a manifest that has eaten its own
//!   provenance is not evidence either.
//!
//! The environment is not redacted, it is **allow-listed** (see the metadata capture in
//! [`crate::report`]) — a value that is never read cannot leak. Redaction is the backstop for the
//! strings that have to be captured as written to be useful at all.
//!
//! Redaction is idempotent: `[redacted]` carries no credential shape, so applying it again is a
//! no-op — which matters because capture runs once and a report can be re-serialized from a
//! manifest that was already redacted.

/// The marker substituted for a user's home directory.
const HOME_MARKER: &str = "~";

/// Replacement text for any value judged to be a credential.
pub const REDACTED: &str = "[redacted]";

/// Key names whose value is a credential.
///
/// Lower-case; compared against a lower-cased copy of the input. Compound spellings are listed in
/// full because the matcher requires a word boundary before the key: in `api_key=…` the candidate
/// `key` is rejected (a `_` precedes it) and `api_key` is the match.
const SECRET_KEYS: &[&str] = &["password", "passwd", "pwd", "token", "secret", "api_key", "apikey", "access_key", "accesskey", "secret_key", "private_key", "credential", "credentials", "authorization", "bearer", "auth", "session", "session_id", "cookie", "csrf", "xsrf", "signature", "x-api-key", "x-amz-credential", "x-goog-credential", "webhook", "conn", "connection_string", "dsn"];

/// Keys whose value is a whole field rather than one word — `Authorization: Bearer <opaque>` has a
/// space inside the credential, so stopping at the space would keep half of it.
const VALUE_SPANS_FIELD: &[&str] = &["authorization", "bearer", "cookie", "private_key", "connection_string", "dsn", "credentials"];

/// Word prefixes that identify a credential by shape alone, with the minimum total length that
/// separates them from an ordinary word starting the same way.
const TOKEN_PREFIXES: &[(&str, usize)] = &[("ghp_", 36), ("gho_", 36), ("ghu_", 36), ("ghs_", 36), ("ghr_", 36), ("github_pat_", 40), ("xox", 20), ("akia", 20), ("asia", 20), ("aiza", 35), ("ya29.", 40), ("sk-", 24), ("rk_live_", 24), ("pk_live_", 24), ("glpat-", 26), ("dsk_", 30)];

/// The marker that opens a PEM private-key or certificate block.
const PEM_MARKER: &str = "-----BEGIN";

/// Redact anything in this text that could be a credential.
///
/// Applied to every free-text string the manifest captures. Idempotent.
#[must_use]
pub fn redact(text: &str) -> String {
	let without_pem = redact_pem(text);
	let without_tokens = redact_tokens(&without_pem);
	let without_assignments = redact_assignments(&without_tokens);
	let without_userinfo = redact_url_userinfo(&without_assignments);
	redact_home(&without_userinfo)
}

/// Replace a private-key block with [`REDACTED`].
///
/// A PEM body has no shape a single-line scanner can follow to its end, so everything from the
/// marker to the end of the string goes. That over-redacts the remainder of the line, which is the
/// right direction to fail for a string that should not have been in a manifest at all.
#[must_use]
pub fn redact_pem(text: &str) -> String {
	match text.find(PEM_MARKER) {
		Some(index) => format!("{}{REDACTED}", &text[..index]),
		None => text.to_string(),
	}
}

/// Replace the user-info of every URL with [`REDACTED`].
///
/// `https://weft_reader:S3cr3t@metrics.internal/db` becomes `https://[redacted]@metrics.internal/db`
/// — the host and path are what makes a run reproducible, the account is not.
#[must_use]
pub fn redact_url_userinfo(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	let mut cursor = 0usize;
	while let Some(relative) = text[cursor..].find("://") {
		let authority_start = cursor + relative + 3;
		out.push_str(&text[cursor..authority_start]);
		// An authority runs to the first path/query/fragment separator, delimiter or whitespace.
		let authority_end = text[authority_start..].find(['/', '?', '#', ' ', '\t', '\n', '"', '\'', ')', ']', '>', ',']).map_or(text.len(), |offset| authority_start + offset);
		let authority = &text[authority_start..authority_end];
		match authority.rfind('@') {
			Some(at) => {
				out.push_str(REDACTED);
				out.push('@');
				out.push_str(&authority[at + 1..]);
			}
			None => out.push_str(authority),
		}
		cursor = authority_end;
	}
	out.push_str(&text[cursor..]);
	out
}

/// Replace the value of every `key=value` or `key: value` pair whose key is in `SECRET_KEYS`.
#[must_use]
pub fn redact_assignments(text: &str) -> String {
	let lower = ascii_lower(text);
	let mut out = String::with_capacity(text.len());
	// `emitted` tracks how far `text` has been copied; `scan` how far `lower` has been searched.
	// Both are byte indices into the same string, since lower-casing preserves byte length for the
	// ASCII and UTF-8 we expect (see the round-trip test).
	let mut emitted = 0usize;
	let mut scan = 0usize;
	while let Some((index, key_len)) = find_secret_key(&lower, scan) {
		match separator_end(&lower, index + key_len) {
			Some(value_start) => {
				// The value is already a marker: this string has been through here before, and
				// redacting what follows the marker is what a second pass must not do.
				if text[value_start..].starts_with(REDACTED) {
					scan = value_start + REDACTED.len();
					continue;
				}
				let spans_field = VALUE_SPANS_FIELD.contains(&&lower[index..index + key_len]);
				out.push_str(&text[emitted..value_start]);
				out.push_str(REDACTED);
				let end = skip_value(text, value_start, spans_field);
				emitted = end;
				scan = end;
			}
			// A secret-looking word with no separator after it is prose, not a credential.
			None => scan = index + key_len,
		}
	}
	out.push_str(&text[emitted..]);
	out
}

/// A lower-case copy of `text` that is **guaranteed to have the same byte length**, so every index
/// found in it is a valid index into `text` on a character boundary.
///
/// `str::to_lowercase` is not safe here: it can grow a string (`\u{130}` becomes two characters),
/// which would shift indices into the middle of a code point. Every key we look for is ASCII, so
/// folding ASCII alone is enough.
fn ascii_lower(text: &str) -> String {
	text.chars().map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c }).collect()
}

/// The earliest `SECRET_KEYS` hit at a word boundary at or after `from`, as `(index, length)`.
///
/// On a tie the longest key wins, so `api_key=` is matched by `api_key`.
fn find_secret_key(lower: &str, from: usize) -> Option<(usize, usize)> {
	let bytes = lower.as_bytes();
	let mut best: Option<(usize, usize)> = None;
	for key in SECRET_KEYS {
		let mut haystack = from;
		while let Some(relative) = lower[haystack..].find(key) {
			let index = haystack + relative;
			haystack = index + key.len().max(1);
			// `_` continues an identifier (`token_count` is a count, not a credential) but `-` does
			// not: a hyphen is how a command line spells a key name, so `--api-token=` must match
			// on `token` rather than needing every hyphenated compound listed.
			let before_ok = index.checked_sub(1).is_none_or(|previous| !(bytes[previous].is_ascii_alphanumeric() || bytes[previous] == b'_'));
			if !before_ok {
				continue;
			}
			let after_ok = matches!(bytes.get(index + key.len()), None | Some(b'=') | Some(b':') | Some(b' ') | Some(b'\t'));
			if !after_ok {
				continue;
			}
			best = Some(match best {
				Some((best_index, best_len)) if best_index < index || (best_index == index && best_len >= key.len()) => (best_index, best_len),
				_ => (index, key.len()),
			});
			break;
		}
	}
	best
}

/// The byte index at which the value begins, when the key ending at `after_key` is followed by `=`
/// or `:`; `None` when the key is bare.
fn separator_end(lower: &str, after_key: usize) -> Option<usize> {
	let rest = lower.get(after_key..)?;
	let trimmed = rest.trim_start_matches([' ', '\t']);
	let separator = *trimmed.as_bytes().first()?;
	if separator != b'=' && separator != b':' {
		return None;
	}
	let after_separator = trimmed.get(1..)?;
	let leading = after_separator.len() - after_separator.trim_start_matches([' ', '\t']).len();
	Some(after_key + (rest.len() - trimmed.len()) + 1 + leading)
}

/// The byte index at which the redacted value ends.
///
/// A quoted value runs to its closing quote. Otherwise an ordinary key's value stops at the next
/// field separator, and a [`VALUE_SPANS_FIELD`] key's value keeps going through spaces to the end
/// of the field — the separator set for that case excludes whitespace, so `Bearer <token>` goes as
/// one piece.
fn skip_value(text: &str, from: usize, spans_field: bool) -> usize {
	let rest = &text[from..];
	if let Some(quote @ ('"' | '\'')) = rest.chars().next() {
		return rest[1..].find(quote).map_or(text.len(), |end| from + 1 + end + quote.len_utf8());
	}
	let separators: &[char] = if spans_field { &[',', ';', '&', '"', '\'', ']', ')', '}'] } else { &[' ', '\t', ',', ';', '&', '"', '\'', ']', ')', '}'] };
	rest.find(separators).map_or(text.len(), |end| from + end)
}

/// Replace anything shaped like a provider token, wherever it appears.
#[must_use]
pub fn redact_tokens(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	for (word, separator) in split_words(text) {
		out.push_str(&redact_word(word));
		out.push_str(separator);
	}
	out
}

/// Redact one delimited word when it matches a known token shape.
fn redact_word(word: &str) -> String {
	let lowered = word.to_lowercase();
	if TOKEN_PREFIXES.iter().any(|(prefix, minimum)| lowered.starts_with(prefix) && word.chars().count() >= *minimum) {
		return REDACTED.to_string();
	}
	// A JWT is three base64url segments and always starts `eyJ`, the encoding of `{"`.
	if lowered.starts_with("eyj") && word.matches('.').count() == 2 && word.chars().count() >= 32 {
		return REDACTED.to_string();
	}
	word.to_string()
}

/// Split into `(word, following separator)` pairs covering the whole string, where a word is a run
/// of characters that can appear inside a credential and a separator is everything else.
///
/// `=` and `:` are deliberately *not* word characters: `token=ghp_…` must leave `ghp_…` standing
/// alone so the shape rule sees it.
fn split_words(text: &str) -> Vec<(&str, &str)> {
	let is_word = |byte: u8| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b'+');
	let bytes = text.as_bytes();
	let mut parts = Vec::new();
	let mut index = 0usize;
	while index < bytes.len() {
		let separator_start = if is_word(bytes[index]) {
			let start = index;
			while index < bytes.len() && is_word(bytes[index]) {
				index += 1;
			}
			let separator_from = index;
			while index < bytes.len() && !is_word(bytes[index]) {
				index += 1;
			}
			parts.push((&text[start..separator_from], &text[separator_from..index]));
			continue;
		} else {
			index
		};
		while index < bytes.len() && !is_word(bytes[index]) {
			index += 1;
		}
		parts.push(("", &text[separator_start..index]));
	}
	parts
}

/// Replace the home directory with `HOME_MARKER`.
///
/// A working directory under the home tree is useful provenance; the account name inside
/// `/home/penelope/…` is not, and it is the commonest way a published artifact leaks who ran it.
#[must_use]
pub fn redact_home(text: &str) -> String {
	let homes = ["HOME", "USERPROFILE"].iter().filter_map(|name| std::env::var_os(name)).map(|value| value.to_string_lossy().into_owned()).filter(|home| home.len() > 1);
	let mut out = text.to_string();
	for home in homes {
		if out.contains(&home) {
			out = out.replace(&home, HOME_MARKER);
		}
	}
	out
}

/// Replace `home` with `HOME_MARKER` — the decision [`redact_home`] makes, with the directory
/// passed in so it is testable without mutating the process environment.
#[must_use]
pub fn replace_home(text: &str, home: Option<&str>) -> String {
	match home.filter(|h| h.len() > 1) {
		Some(home) if text.contains(home) => text.replace(home, HOME_MARKER),
		_ => text.to_string(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The strings a manifest is actually full of. None of them may be altered: the point of the
	/// manifest is that a reader can tell one machine from another.
	const ORDINARY: &[&str] = &["AMD Ryzen 9 5900X 12-Core Processor", "AMD EPYC 7763 64-Core Processor", "NVIDIA GeForce RTX 4070 Ti SUPER (vulkan, discrete; f64 shaders: yes)", "NVIDIA 580.82.09", "btrfs", "/mnt/data2", "/var/lib/weftdb", "weft-bench 0.1.0", "rustc 1.99.0 (1a2b3c4d5e6f 2026-08-13)", "weftdb-0.1.0 --workload upsample_interpolate --reps 7 --resolution minutes", "performance", "ssd", "interpolation-heavy-irregular", "scaled_i64", "2026-10-25T09:14:02.338817477Z", "compression-coldstart"];

	#[test]
	fn ordinary_provenance_is_left_alone() {
		for text in ORDINARY {
			assert_eq!(redact(text), *text, "redaction altered captured provenance: {text:?}");
		}
	}

	#[test]
	fn a_url_password_goes_and_the_host_stays() {
		assert_eq!(redact("https://weft_reader:S3cr3t@metrics.internal/ingest"), format!("https://{REDACTED}@metrics.internal/ingest"));
		assert_eq!(redact("load --url http://localhost:8086/api"), "load --url http://localhost:8086/api");
		assert_eq!(redact("postgres://svc@db.internal:5432/weft"), format!("postgres://{REDACTED}@db.internal:5432/weft"));
	}

	#[test]
	fn a_secret_named_value_goes_whatever_the_separator() {
		assert_eq!(redact("--api-token=gheXaMplEValue"), format!("--api-token={REDACTED}"));
		assert_eq!(redact("token: \"quoted secret\" tail"), format!("token: {REDACTED} tail"));
		assert_eq!(redact("X-Api-Key: abc123def456"), format!("X-Api-Key: {REDACTED}"));
		assert_eq!(redact("password = hunter2; reps=7"), format!("password = {REDACTED}; reps=7"));
	}

	#[test]
	fn a_secret_sounding_word_with_no_value_survives() {
		// "token" is a word in this project's vocabulary; only an assignment to it is a credential.
		for text in ["token", "the token bucket profile", "session", "auth"] {
			assert_eq!(redact(text), text, "prose containing {text:?} was redacted");
		}
	}

	#[test]
	fn a_bearer_value_leaves_whole() {
		// The credential itself contains a space, so stopping at the space would keep half of it.
		let redacted = redact("Authorization: Bearer hunter2 secret value, reps 7");
		assert_eq!(redacted, format!("Authorization: {REDACTED}, reps 7"));
	}

	#[test]
	fn provider_token_shapes_are_recognised_without_a_key_name() {
		let github = format!("ghp_{}", "a".repeat(36));
		assert_eq!(redact(&format!("pushed with {github}")), format!("pushed with {REDACTED}"));
		let aws = format!("AKIA{}", "A".repeat(16));
		assert_eq!(redact(&format!("--access {aws}")), format!("--access {REDACTED}"));
		let jwt = format!("{}.{}.{}", "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9", "eyJzdWIiOiIxMjM0NTY3ODkw", "c2lnbmF0dXJlLWJ5dGVz");
		assert_eq!(redact(&format!("--header {jwt}")), format!("--header {REDACTED}"));
	}

	#[test]
	fn a_short_word_with_a_token_prefix_is_still_a_word() {
		// `sk-` opens "sk-", "sk-1" and a model name; a real key is long.
		for text in ["sk-", "sk-measured", "ya29"] {
			assert_eq!(redact(text), text, "{text:?} is not a credential");
		}
	}

	#[test]
	fn a_private_key_takes_the_rest_of_the_line_with_it() {
		assert_eq!(redact("--key -----BEGIN OPENSSH PRIVATE KEY-----b3BlbnNzaA"), format!("--key {REDACTED}"));
	}

	#[test]
	fn the_home_directory_becomes_a_tilde() {
		assert_eq!(replace_home("/home/penelope/bench/out.json", Some("/home/penelope")), "~/bench/out.json");
		assert_eq!(replace_home("/mnt/data2/bench", Some("/home/penelope")), "/mnt/data2/bench");
		assert_eq!(replace_home("/home/penelope/x", None), "/home/penelope/x");
		// A one-character home would match every path separator, so it is refused.
		assert_eq!(replace_home("/x", Some("/")), "/x");
	}

	#[test]
	fn redaction_is_idempotent() {
		let corpus = ORDINARY.iter().map(|s| s.to_string()).chain(["https://u:p@h/db".to_string(), "token=abc".to_string(), format!("ghp_{}", "b".repeat(36)), "-----BEGIN CERTIFICATE-----abc".to_string()]);
		for text in corpus {
			let once = redact(&text);
			assert_eq!(redact(&once), once, "redaction is not idempotent for {text:?}");
		}
	}

	#[test]
	fn non_ascii_text_is_indexed_by_bytes_safely() {
		// Lower-casing must not shift the byte indices used to slice the original.
		assert_eq!(redact("caf\u{e9} \u{301}de --token=abc123"), format!("caf\u{e9} \u{301}de --token={REDACTED}"));
		assert_eq!(redact("\u{130}stanbul --password=x"), format!("\u{130}stanbul --password={REDACTED}"));
	}
}
