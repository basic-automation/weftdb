//! The one error type: a refusal from the server's envelope, or a failure on the way to it.

use std::{fmt, time::Duration};

use serde::Deserialize;

/// What kind of failure an [`Error`] is, grouped by what a caller does about it.
///
/// The server's own `code` (on [`Error::code`]) is finer: branch on it when one family is
/// not enough, e.g. `aspect_not_found` against `route_not_found`, both [`ErrorKind::NotFound`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
	/// `4xx` the caller can fix: a malformed body or parameter, a value the aspect cannot hold.
	BadRequest,
	/// `401`: no key, or one the server does not know.
	Unauthenticated,
	/// `403`: the key's role lacks the route's scope, or a guard refused the request.
	Forbidden,
	/// `404`: no such route, aspect, run or backup.
	NotFound,
	/// `409`/`410`: the request conflicts with the store's state.
	Conflict,
	/// `503 overloaded`: the admission class is full; [`Error::retry_after`] says when to retry.
	Overloaded,
	/// Any other `5xx`.
	Server,
	/// An answer that does not match what was asked for, e.g. a bench run whose bytes do not
	/// hash to its id.
	Integrity,
	/// The request never got an HTTP answer: connection, TLS or timeout.
	Transport,
	/// A `2xx` answer this client could not decode.
	Decode,
	/// An argument refused before anything was sent.
	InvalidInput,
}

impl ErrorKind {
	/// The family a refusal's status and envelope `code` belong to.
	#[must_use]
	pub fn from_status(status: u16, code: Option<&str>) -> Self {
		match (status, code) {
			(_, Some("overloaded")) => Self::Overloaded,
			(401, _) => Self::Unauthenticated,
			(403, _) => Self::Forbidden,
			(404, _) => Self::NotFound,
			(409 | 410, _) => Self::Conflict,
			(500.., _) => Self::Server,
			_ => Self::BadRequest,
		}
	}
}

/// A failed request.
///
/// For a refusal every field of the server's envelope is kept: `code` to branch on,
/// `details` and `limit` for the specifics, `committed` for a write that may have landed,
/// and `request_id`, which is the line to quote when asking an operator about it.
#[derive(Debug)]
pub struct Error(Box<Inner>);

/// The fields, boxed so a `Result<T, Error>` stays one pointer wide on the error side.
#[derive(Debug)]
struct Inner {
	kind: ErrorKind,
	message: String,
	status: Option<u16>,
	code: Option<String>,
	details: Option<serde_json::Value>,
	limit: Option<u64>,
	committed: Option<serde_json::Value>,
	request_id: Option<String>,
	retry_after: Option<Duration>,
	source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

/// The server's error envelope, as it arrives.
#[derive(Debug, Default, Deserialize)]
pub struct Envelope {
	#[serde(default)]
	pub error: Option<String>,
	#[serde(default)]
	pub code: Option<String>,
	#[serde(default)]
	pub details: Option<serde_json::Value>,
	#[serde(default)]
	pub limit: Option<u64>,
	#[serde(default)]
	pub committed: Option<serde_json::Value>,
	#[serde(default)]
	pub request_id: Option<String>,
}

impl Error {
	pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
		Self(Box::new(Inner { kind, message: message.into(), status: None, code: None, details: None, limit: None, committed: None, request_id: None, retry_after: None, source: None }))
	}

	pub(crate) fn invalid(message: impl Into<String>) -> Self {
		Self::new(ErrorKind::InvalidInput, message)
	}

	pub(crate) fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
		self.0.source = Some(Box::new(source));
		self
	}

	/// A refusal built from its status, headers' request id and `Retry-After`, and body.
	pub(crate) fn refusal(status: u16, reason: &str, header_request_id: Option<String>, retry_after: Option<Duration>, body: &[u8]) -> Self {
		let envelope = serde_json::from_slice::<Envelope>(body).unwrap_or_else(|_| {
			let text = String::from_utf8_lossy(body).trim().to_owned();
			Envelope { error: Some(if text.is_empty() { reason.to_owned() } else { text }), ..Envelope::default() }
		});
		let kind = ErrorKind::from_status(status, envelope.code.as_deref());
		Self(Box::new(Inner { kind, message: envelope.error.unwrap_or_else(|| reason.to_owned()), status: Some(status), code: envelope.code, details: envelope.details, limit: envelope.limit, committed: envelope.committed, request_id: envelope.request_id.or(header_request_id), retry_after, source: None }))
	}

	/// The failure's family.
	#[must_use]
	pub fn kind(&self) -> ErrorKind {
		self.0.kind
	}

	/// The server's sentence, or this client's description of what went wrong.
	#[must_use]
	pub fn message(&self) -> &str {
		&self.0.message
	}

	/// The HTTP status, for a refusal.
	#[must_use]
	pub fn status(&self) -> Option<u16> {
		self.0.status
	}

	/// The envelope's `code` (`aspect_not_found`, `value_not_representable`, …).
	#[must_use]
	pub fn code(&self) -> Option<&str> {
		self.0.code.as_deref()
	}

	/// The envelope's `details`, when the code carries any.
	#[must_use]
	pub fn details(&self) -> Option<&serde_json::Value> {
		self.0.details.as_ref()
	}

	/// The cap a `result_too_large` or `payload_too_large` refusal names.
	#[must_use]
	pub fn limit(&self) -> Option<u64> {
		self.0.limit
	}

	/// For a refused write, what the server says did commit.
	#[must_use]
	pub fn committed(&self) -> Option<&serde_json::Value> {
		self.0.committed.as_ref()
	}

	/// The request id from the body, or else the `x-request-id` header.
	#[must_use]
	pub fn request_id(&self) -> Option<&str> {
		self.0.request_id.as_deref()
	}

	/// How long the server asked the caller to wait, from `Retry-After`.
	#[must_use]
	pub fn retry_after(&self) -> Option<Duration> {
		self.0.retry_after
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match (self.0.status, &self.0.code) {
			(Some(status), Some(code)) => write!(f, "{status} {code}: {}", self.0.message)?,
			(Some(status), None) => write!(f, "{status}: {}", self.0.message)?,
			_ => f.write_str(&self.0.message)?,
		}
		if let Some(id) = &self.0.request_id {
			write!(f, " (request {id})")?;
		}
		Ok(())
	}
}

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self.0.source.as_deref().map(|source| source as &(dyn std::error::Error + 'static))
	}
}

/// The crate's `Result`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_status_and_code_pick_the_family() {
		assert_eq!(ErrorKind::from_status(503, Some("overloaded")), ErrorKind::Overloaded);
		assert_eq!(ErrorKind::from_status(503, Some("unconfigured")), ErrorKind::Server);
		assert_eq!(ErrorKind::from_status(401, Some("unauthenticated")), ErrorKind::Unauthenticated);
		assert_eq!(ErrorKind::from_status(403, None), ErrorKind::Forbidden);
		assert_eq!(ErrorKind::from_status(404, Some("aspect_not_found")), ErrorKind::NotFound);
		assert_eq!(ErrorKind::from_status(410, None), ErrorKind::Conflict);
		assert_eq!(ErrorKind::from_status(422, Some("value_not_representable")), ErrorKind::BadRequest);
	}

	#[test]
	fn an_envelope_keeps_every_field() {
		let body = br#"{"error":"too many","code":"result_too_large","limit":10,"details":{"x":1},"committed":{"rows":3},"request_id":"req-body"}"#;
		let error = Error::refusal(413, "Payload Too Large", Some("req-header".into()), None, body);
		assert_eq!(error.kind(), ErrorKind::BadRequest);
		assert_eq!(error.code(), Some("result_too_large"));
		assert_eq!(error.limit(), Some(10));
		assert_eq!(error.details().unwrap()["x"], 1);
		assert_eq!(error.committed().unwrap()["rows"], 3);
		assert_eq!(error.request_id(), Some("req-body"), "the body's id wins; it is the one in the log line");
		assert_eq!(error.to_string(), "413 result_too_large: too many (request req-body)");
	}

	#[test]
	fn a_body_that_is_not_an_envelope_still_says_something() {
		let error = Error::refusal(502, "Bad Gateway", Some("req-h".into()), None, b"upstream died\n");
		assert_eq!(error.kind(), ErrorKind::Server);
		assert_eq!(error.message(), "upstream died");
		assert_eq!(error.code(), None);
		assert_eq!(error.request_id(), Some("req-h"));
		assert_eq!(Error::refusal(502, "Bad Gateway", None, None, b"").message(), "Bad Gateway");
	}
}
