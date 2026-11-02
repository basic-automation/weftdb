//! Secret hygiene for the server's own output (SEC-10).
//!
//! The server prints and logs values an operator configured — startup lines,
//! refusal reasons, daemon reports. Some of those values can carry a
//! credential: an OTLP collector endpoint is a URL, and a URL can carry
//! userinfo (`https://user:password@collector:4317`), the shape collectors
//! use for basic auth. Printing the endpoint as configured would put that
//! password in the server's stdout and in every log the stdout lands in.
//!
//! The rule this module exists to enforce: **a value that can carry a
//! credential is only ever printed with the credential part redacted.**
//! Redaction is display-only — the exporter still receives the endpoint
//! exactly as configured, userinfo included, because the collector's
//! basic auth rides on it. What changes is what the server says about it.
//!
//! This is the server-surface counterpart of `weft-bench`'s run-manifest
//! redaction (EVI-3/SOA-2, which SEC-10's OTLP half was split out of
//! SOA-14 to join): the same `[redacted]` marker and the same URL-userinfo
//! semantics, kept in this crate because the bench harness is not a
//! dependency the server may take. Redaction is idempotent: `[redacted]`
//! carries no credential shape, so applying it again is a no-op.

/// Replacement text for any value judged to be a credential.
pub const REDACTED: &str = "[redacted]";

/// Replace the userinfo of every URL in `text` with [`REDACTED`].
///
/// `https://weft_reader:S3cr3t@metrics.internal/db` becomes
/// `https://[redacted]@metrics.internal/db` — the scheme, host and path
/// are what make a printed endpoint useful for diagnosis; the account
/// and password are not. Text without a `scheme://` authority, and
/// authorities without an `@`, pass through unchanged, as does an `@`
/// later in the path (it is not userinfo once the authority has ended).
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_endpoint_without_userinfo_passes_through_unchanged() {
		assert_eq!(redact_url_userinfo("http://localhost:4317"), "http://localhost:4317");
		assert_eq!(redact_url_userinfo("https://collector.internal:4317/v1/traces"), "https://collector.internal:4317/v1/traces");
		assert_eq!(redact_url_userinfo(""), "");
		assert_eq!(redact_url_userinfo("no url at all"), "no url at all");
	}

	#[test]
	fn userinfo_is_replaced_and_the_rest_of_the_url_survives() {
		assert_eq!(redact_url_userinfo("https://weft_reader:S3cr3t@metrics.internal:4317/db"), "https://[redacted]@metrics.internal:4317/db");
		// A user with no password is still an account name: redact it too.
		assert_eq!(redact_url_userinfo("http://operator@collector:4317"), "http://[redacted]@collector:4317");
		// A password containing an `@` redacts up to the last one, like a URL parser reads it.
		assert_eq!(redact_url_userinfo("https://user:p@ss@host:4317"), "https://[redacted]@host:4317");
	}

	#[test]
	fn every_url_in_a_longer_message_is_redacted() {
		let message = "primary https://a:b@one.internal:4317 down, falling back to http://c:d@two.internal:4317 (retry)";
		assert_eq!(redact_url_userinfo(message), "primary https://[redacted]@one.internal:4317 down, falling back to http://[redacted]@two.internal:4317 (retry)");
	}

	#[test]
	fn an_at_sign_outside_the_authority_is_not_userinfo() {
		assert_eq!(redact_url_userinfo("http://host:4317/p@th"), "http://host:4317/p@th");
		assert_eq!(redact_url_userinfo("http://host:4317/?q=a@b"), "http://host:4317/?q=a@b");
		// No `://`, no authority: a bare `user@host` is prose here, not a URL.
		assert_eq!(redact_url_userinfo("user@host"), "user@host");
	}

	#[test]
	fn redaction_is_idempotent() {
		let once = redact_url_userinfo("https://user:pass@host:4317");
		assert_eq!(redact_url_userinfo(&once), once);
	}
}
