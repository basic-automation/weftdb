//! One error contract for the whole HTTP surface (1.0 plan **X-ERR**, resolution **C6**).
//!
//! Before this module, each endpoint group rendered its own shape: the storage endpoints
//! had [`StorageError`](crate::storage) with a `code` on two variants only, the compute
//! endpoints had a two-variant `ApiError` with no `code` at all, and the loopback guard
//! wrote its `{"error": …}` by hand. The same mistake therefore looked different
//! depending on which route it came from, and a client could not branch on anything
//! machine-readable. This module replaces all three with one type and one table.
//!
//! **The envelope** is `{error, code, details?, limit?, request_id?, committed?}`:
//!
//! - `error` — a message written for the operator of the *caller*: what to change. For a
//!   `5xx` it is one fixed sentence per code, never the cause (see redaction below).
//! - `code` — always present, always from [`Code`]. This is what a client branches on;
//!   the message is allowed to change, the code is not.
//! - `details` — optional structured facts (a rejected tag, an offending row index).
//! - `limit` — the cap a `413`/`422` hit, as a number rather than buried in prose.
//! - `request_id` — the same id the `x-request-id` header carries, injected by
//!   [`trace::request_span`](crate::trace) so an error body and a log line name each other.
//! - `committed` — present on a write whose outcome the server can state: `false` on
//!   `commit_failed`, so a caller knows a retry is safe.
//!
//! **The code table** is the contract, not this module's opinion: [`Code`] is the table in
//! `docs/release/1.0-plan.md` §4 (C6), and a test asserts every code's status against it.
//! The set is *open* — later slices (SEC, ROB, TAG) add codes for conditions that have no
//! route yet — and `#[non_exhaustive]` says so to downstream matches. Codes the tracks used
//! for the same condition (`invalid_request`, `unauthorized`, `store_unconfigured`,
//! `maintenance_busy`, `limit_exceeded`, `panic`, `segment_invalid`) are gone; the table
//! has one name per condition, and `tests/error_contract.rs` keeps them out.
//!
//! **`5xx` details are logged, not sent.** A `500` body used to carry the underlying
//! libSQL, filesystem or Arrow message, which names absolute paths and internal shapes to
//! anyone who can send a request, and tells the operator nothing they can act on. A `5xx`
//! through this module writes the real cause to `tracing::error!` with the code and keeps
//! the id-linkable generic sentence on the wire. The plan's exception — full detail on
//! `operator_detail` routes — needs the route registry that SEC-3 owns (W3), so until that
//! exists the redaction is unconditional.
//!
//! **Classification is by type, not by message.** The previous code matched
//! `err.to_string()` against substrings (`"seal failed"`, `"no declared schema"`,
//! `"parquet error"`) to guess a status class, which meant an unrelated message that
//! happened to contain one of those words was reclassified, and a wording change in another
//! crate silently moved a status code. [`classify`] downcasts instead. The one substring
//! rule that remains is marked with the slice that will replace it.

use std::borrow::Cow;

use axum::{
	extract::{rejection::JsonRejection, FromRequest, FromRequestParts, Query, Request}, http::{request::Parts, HeaderName, StatusCode}, response::{IntoResponse, Response}, Json
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

/// A machine-readable error condition, from the single table in `docs/release/1.0-plan.md` §4 (C6).
///
/// Every HTTP error this server produces names exactly one of these, and the status comes from
/// the code rather than from the call site, so one condition cannot be a `409` on one route and
/// a `503` on another.
///
/// `#[non_exhaustive]` because the set is open by design: the security, durability and tags
/// lanes add codes for conditions their slices introduce. A client matches on the string
/// form, not on this enum, so adding a code is not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Code {
	// ── 400 ────────────────────────────────────────────────────────────────────────
	/// The request is malformed in a way none of the more specific codes describes.
	BadRequest,
	/// A JSON body did not parse, or was not an object of the expected shape.
	InvalidJson,
	/// The aspect name is not one the store can address as a file name.
	InvalidAspectName,
	/// A decimal literal is outside the plain-form domain the store accepts.
	DecimalOutOfRange,
	/// A value that must be finite (`NaN`, `inf`) was supplied or produced.
	NotFinite,
	/// Credentials were put in the query string, where they land in logs and proxies.
	CredentialsInQuery,
	/// A tag name or value is outside the tag grammar.
	InvalidTag,
	/// A selector matched more than one series where exactly one was required.
	AmbiguousSeries,
	/// The request asked for tags on a store whose layout predates them.
	TagsUnsupported,
	/// The request would create or touch more series than the configured cap allows.
	SeriesLimit,
	// ── 401 ────────────────────────────────────────────────────────────────────────
	/// No credentials, or credentials this server does not accept (`WWW-Authenticate`).
	Unauthenticated,
	// ── 403 ────────────────────────────────────────────────────────────────────────
	/// Authenticated, but not permitted for this route or role.
	Forbidden,
	/// The `Host` header names something this loopback-bound server will not answer to.
	HostNotAllowed,
	/// A state-changing request from a web origin the server does not trust.
	CrossOriginWrite,
	/// A request that arrived through a proxy without the authentication that requires.
	ProxiedRequestRequiresAuth,
	// ── 404 ────────────────────────────────────────────────────────────────────────
	/// No route matches the method and path.
	RouteNotFound,
	/// The addressed aspect does not exist in this store.
	AspectNotFound,
	// ── 405 ────────────────────────────────────────────────────────────────────────
	/// The path is a route but the method is not one it serves (`Allow` header).
	MethodNotAllowed,
	// ── 408 ────────────────────────────────────────────────────────────────────────
	/// The request body did not arrive within the body deadline.
	BodyTimeout,
	// ── 409 ────────────────────────────────────────────────────────────────────────
	/// The request contradicts the aspect's declared schema.
	SchemaConflict,
	/// The name the request would create is already taken.
	AlreadyExists,
	/// Another operation holds the resource; retry later (`Retry-After`).
	Busy,
	/// An `Idempotency-Key` was reused with a different request body.
	IdempotencyConflict,
	// ── 410 ────────────────────────────────────────────────────────────────────────
	/// A cursor refers to a snapshot the server no longer holds.
	CursorStale,
	// ── 413 ────────────────────────────────────────────────────────────────────────
	/// The body exceeds the ingest byte cap (`limit`).
	PayloadTooLarge,
	/// The body carries more rows than the row cap allows.
	TooManyRows,
	// ── 415 ────────────────────────────────────────────────────────────────────────
	/// The body's content type is not one this route decodes.
	UnsupportedMediaType,
	// ── 422 ────────────────────────────────────────────────────────────────────────
	/// The request is well-formed but its result would exceed a response cap (`limit`).
	ResultTooLarge,
	/// A value cannot be represented in the aspect's physical type.
	ValueNotRepresentable,
	// ── 500 ────────────────────────────────────────────────────────────────────────
	/// A server-side failure with no more specific code; the cause is logged, not sent.
	Internal,
	/// A write did not commit, so the store was not modified and retrying is safe
	/// (`committed: false`).
	CommitFailed,
	/// A stored frame could not be decoded (the store's data, not the caller's request).
	FrameUndecodable,
	/// A stored row's series identity does not match the aspect's binding.
	SeriesBindingMismatch,
	// ── 503 ────────────────────────────────────────────────────────────────────────
	/// The route needs a subsystem this server was started without.
	Unconfigured,
	/// The process is up but has not finished becoming able to serve.
	NotReady,
	/// The store on disk uses a format this binary will not open.
	IncompatibleStore,
	/// The audit log could not be written, and the route requires that it was.
	AuditUnavailable,
	/// Admission control rejected the request to protect the store (`Retry-After`).
	Overloaded,
	/// The server cannot say whether a write committed; retrying needs care.
	OutcomeUnknown,
	/// The store is poisoned and refuses work until an operator intervenes.
	Poisoned,
	// ── 504 ────────────────────────────────────────────────────────────────────────
	/// The query or ingest exceeded its deadline.
	QueryTimeout,
	// ── 507 ────────────────────────────────────────────────────────────────────────
	/// The filesystem cannot accept the write without risking a full disk.
	InsufficientStorage,
}
impl Code {
	/// The wire form of the code: the string a client branches on, exactly as the §4 table
	/// spells it.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::BadRequest => "bad_request",
			Self::InvalidJson => "invalid_json",
			Self::InvalidAspectName => "invalid_aspect_name",
			Self::DecimalOutOfRange => "decimal_out_of_range",
			Self::NotFinite => "not_finite",
			Self::CredentialsInQuery => "credentials_in_query",
			Self::InvalidTag => "invalid_tag",
			Self::AmbiguousSeries => "ambiguous_series",
			Self::TagsUnsupported => "tags_unsupported",
			Self::SeriesLimit => "series_limit",
			Self::Unauthenticated => "unauthenticated",
			Self::Forbidden => "forbidden",
			Self::HostNotAllowed => "host_not_allowed",
			Self::CrossOriginWrite => "cross_origin_write",
			Self::ProxiedRequestRequiresAuth => "proxied_request_requires_auth",
			Self::RouteNotFound => "route_not_found",
			Self::AspectNotFound => "aspect_not_found",
			Self::MethodNotAllowed => "method_not_allowed",
			Self::BodyTimeout => "body_timeout",
			Self::SchemaConflict => "schema_conflict",
			Self::AlreadyExists => "already_exists",
			Self::Busy => "busy",
			Self::IdempotencyConflict => "idempotency_conflict",
			Self::CursorStale => "cursor_stale",
			Self::PayloadTooLarge => "payload_too_large",
			Self::TooManyRows => "too_many_rows",
			Self::UnsupportedMediaType => "unsupported_media_type",
			Self::ResultTooLarge => "result_too_large",
			Self::ValueNotRepresentable => "value_not_representable",
			Self::Internal => "internal",
			Self::CommitFailed => "commit_failed",
			Self::FrameUndecodable => "frame_undecodable",
			Self::SeriesBindingMismatch => "series_binding_mismatch",
			Self::Unconfigured => "unconfigured",
			Self::NotReady => "not_ready",
			Self::IncompatibleStore => "incompatible_store",
			Self::AuditUnavailable => "audit_unavailable",
			Self::Overloaded => "overloaded",
			Self::OutcomeUnknown => "outcome_unknown",
			Self::Poisoned => "poisoned",
			Self::QueryTimeout => "query_timeout",
			Self::InsufficientStorage => "insufficient_storage",
		}
	}

	/// The status this code is sent with. The pairing is fixed by the §4 table and asserted
	/// by `the_table_pairs_every_code_with_the_status_the_plan_gives_it`, so a call site
	/// cannot pick its own status: choosing the code chooses the status.
	#[must_use]
	pub const fn status(self) -> StatusCode {
		match self {
			Self::BadRequest | Self::InvalidJson | Self::InvalidAspectName | Self::DecimalOutOfRange | Self::NotFinite | Self::CredentialsInQuery | Self::InvalidTag | Self::AmbiguousSeries | Self::TagsUnsupported | Self::SeriesLimit => StatusCode::BAD_REQUEST,
			Self::Unauthenticated => StatusCode::UNAUTHORIZED,
			Self::Forbidden | Self::HostNotAllowed | Self::CrossOriginWrite | Self::ProxiedRequestRequiresAuth => StatusCode::FORBIDDEN,
			Self::RouteNotFound | Self::AspectNotFound => StatusCode::NOT_FOUND,
			Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
			Self::BodyTimeout => StatusCode::REQUEST_TIMEOUT,
			Self::SchemaConflict | Self::AlreadyExists | Self::Busy | Self::IdempotencyConflict => StatusCode::CONFLICT,
			Self::CursorStale => StatusCode::GONE,
			Self::PayloadTooLarge | Self::TooManyRows => StatusCode::PAYLOAD_TOO_LARGE,
			Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
			Self::ResultTooLarge | Self::ValueNotRepresentable => StatusCode::UNPROCESSABLE_ENTITY,
			Self::Internal | Self::CommitFailed | Self::FrameUndecodable | Self::SeriesBindingMismatch => StatusCode::INTERNAL_SERVER_ERROR,
			Self::Unconfigured | Self::NotReady | Self::IncompatibleStore | Self::AuditUnavailable | Self::Overloaded | Self::OutcomeUnknown | Self::Poisoned => StatusCode::SERVICE_UNAVAILABLE,
			Self::QueryTimeout => StatusCode::GATEWAY_TIMEOUT,
			Self::InsufficientStorage => StatusCode::INSUFFICIENT_STORAGE,
		}
	}

	/// The one sentence a `5xx` client is allowed to see for this code. The real cause goes
	/// to the log; this is what the body carries. Kept free of anything that names a path,
	/// a subsystem version, or an internal type.
	const fn redacted_message(self) -> &'static str {
		match self {
			Self::Internal => "the server could not complete the request",
			Self::CommitFailed => "the write did not commit and the store was not modified",
			Self::FrameUndecodable => "the server could not decode data it has on disk",
			Self::SeriesBindingMismatch => "the server found stored rows that do not match their series",
			Self::Unconfigured => "no segment store is configured on this server",
			Self::NotReady => "the server is not ready to serve requests yet",
			Self::IncompatibleStore => "the store on disk uses a format this server cannot open",
			Self::AuditUnavailable => "the server could not record the request in its audit log",
			Self::Overloaded => "the server is at its admission limit",
			Self::OutcomeUnknown => "the server cannot say whether the write committed",
			Self::Poisoned => "the store is quarantined and needs an operator",
			Self::QueryTimeout => "the server gave up on the operation at its deadline",
			Self::InsufficientStorage => "the server's filesystem cannot take the write safely",
			// A 4xx message is written for the caller and is never a cause string, so it is
			// sent as the call site wrote it.
			_ => "the request was refused",
		}
	}
}

/// The `request_id` carrier between [`ApiError::into_response`] and
/// [`trace::request_span`](crate::trace), which is the only place that knows the id. The
/// marker is set on the response, consumed (and removed) by the middleware, which adds
/// `request_id` to the JSON body. It is internal: it never survives the middleware, so a
/// client sees the envelope and the `x-request-id` header, not this.
pub(crate) const ENVELOPE_MARKER: HeaderName = HeaderName::from_static("x-weft-error-envelope");

/// One error on the HTTP surface: a [`Code`], a message, and the optional envelope fields.
///
/// Handlers return it (`Result<…, ApiError>`) and axum renders it. The four constructors
/// cover almost every call site; the `with_*` builders add the envelope's optional fields.
#[derive(Debug)]
pub struct ApiError {
	code: Code,
	message: String,
	details: Option<Value>,
	limit: Option<u64>,
	committed: Option<bool>,
	retry_after: Option<u32>,
	/// A `5xx` cause to log, kept off the wire by [`redacted_message`](Code::redacted_message).
	cause: Option<String>,
}

impl ApiError {
	/// An error with the code's status and a message written for the caller.
	#[must_use]
	pub fn new(code: Code, message: impl Into<String>) -> Self {
		Self { code, message: message.into(), details: None, limit: None, committed: None, retry_after: None, cause: None }
	}

	/// Attach structured facts to the envelope (`details`) — a rejected name, a row index.
	#[must_use]
	pub fn with_details(mut self, details: impl Into<Value>) -> Self {
		self.details = Some(details.into());
		self
	}

	/// Name the cap the request hit (`limit`), so a client reads the number instead of
	/// parsing it out of the sentence.
	#[must_use]
	pub const fn with_limit(mut self, limit: u64) -> Self {
		self.limit = Some(limit);
		self
	}

	/// Send `Retry-After`. The §4 table pairs it with `busy` and `overloaded`; the value is
	/// the waiting budget the handler actually used, not an invented backoff.
	#[must_use]
	pub const fn with_retry_after(mut self, seconds: u32) -> Self {
		self.retry_after = Some(seconds);
		self
	}

	/// A write that did not commit: the body says `committed: false`, which is what makes a
	/// retry obviously safe without the caller reasoning about the store.
	#[must_use]
	pub fn commit_failed(message: impl Into<String>) -> Self {
		let mut err = Self::new(Code::CommitFailed, message);
		err.committed = Some(false);
		err
	}

	/// A server-side failure: `detail` is the cause (an `anyhow` chain, a driver message).
	/// It is logged under its code and never reaches the client, which gets
	/// [`Code::redacted_message`].
	#[must_use]
	pub fn internal(detail: impl std::fmt::Display) -> Self {
		let detail = detail.to_string();
		tracing::error!(code = Code::Internal.as_str(), cause = %detail, "request failed on the server side");
		Self { code: Code::Internal, message: String::new(), details: None, limit: None, committed: None, retry_after: None, cause: Some(detail) }
	}

	/// A caller-side problem the table has no more specific name for.
	pub fn bad_request(message: impl Into<String>) -> Self {
		Self::new(Code::BadRequest, message)
	}

	/// The aspect in the path is not one this store has.
	pub fn aspect_not_found(message: impl Into<String>) -> Self {
		Self::new(Code::AspectNotFound, message)
	}

	/// The route asked for something this server was started without.
	#[must_use = "a refusal is returned to the caller; constructing and dropping one does nothing"]
	pub fn unconfigured() -> Self {
		Self::new(Code::Unconfigured, "no segment store is configured on this server")
	}

	/// The request collides with the same thing existing, under a different definition.
	pub fn schema_conflict(message: impl Into<String>) -> Self {
		Self::new(Code::SchemaConflict, message)
	}

	/// The thing the request would create is already there.
	pub fn already_exists(message: impl Into<String>) -> Self {
		Self::new(Code::AlreadyExists, message)
	}

	/// Another operation held what this request needed for longer than it would wait.
	pub fn busy(message: impl Into<String>) -> Self {
		Self::new(Code::Busy, message)
	}

	/// The code this error will be sent with.
	#[must_use]
	pub const fn code(&self) -> Code {
		self.code
	}

	/// The message as the call site wrote it (for a `5xx`, the cause that the body does not
	/// carry) — for logs and tests.
	#[must_use]
	pub fn message(&self) -> &str {
		self.cause.as_deref().unwrap_or(&self.message)
	}

	/// The message that goes on the wire: the call site's for a `4xx`, the code's fixed
	/// sentence for a `5xx`.
	#[must_use]
	fn wire_message(&self) -> Cow<'static, str> {
		if self.code.status().is_server_error() {
			Cow::Borrowed(self.code.redacted_message())
		} else {
			Cow::Owned(self.message.clone())
		}
	}
}

/// The envelope as it goes on the wire. Field order matches the plan's envelope; absent
/// optional fields are absent, not `null`, so a client can tell "no limit applies" from
/// "the limit is zero".
#[derive(Debug, Serialize)]
struct Envelope {
	error: Cow<'static, str>,
	code: &'static str,
	#[serde(skip_serializing_if = "Option::is_none")]
	details: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	limit: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	committed: Option<bool>,
}

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		let status = self.code.status();
		let body = Envelope { error: self.wire_message(), code: self.code.as_str(), details: self.details, limit: self.limit, committed: self.committed };
		let mut response = (status, Json(body)).into_response();
		if let Some(seconds) = self.retry_after {
			response.headers_mut().insert(axum::http::header::RETRY_AFTER, axum::http::HeaderValue::from(seconds));
		}
		// Tell the request middleware this body is ours, so it can add `request_id`.
		response.headers_mut().insert(&ENVELOPE_MARKER, axum::http::HeaderValue::from_static("1"));
		response
	}
}

/// A JSON body extractor that answers a bad body with the envelope.
///
/// axum's own `Json` rejection is already JSON, but it is a different shape (no `code`), and
/// its text is a serde message with column offsets. Handlers take
/// `StrictJson(body): StrictJson<MyBody>` so the extracted value binds exactly as before.
#[derive(Debug)]
pub struct StrictJson<T>(pub T);

impl<T, S> FromRequest<S> for StrictJson<T>
where
	T: DeserializeOwned,
	S: Send + Sync,
{
	type Rejection = ApiError;

	async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
		let rejection = match Json::<T>::from_request(request, state).await {
			Ok(Json(value)) => return Ok(Self(value)),
			Err(rejection) => rejection,
		};
		// Two different complaints from one extractor, and the table keeps them apart: the
		// client either sent the wrong kind of document, or sent a document it cannot read.
		//
		// `JsonRejection` is a non-exhaustive composite enum upstream, so every case that is not
		// a content-type complaint — a syntax error, a body that does not match this endpoint's
		// shape, a body the connection cut short — is reported as a bad body. The server-side
		// byte cap arrives with D-S14's `WEFT_MAX_INGEST_BYTES` and owns 413 from then on.
		let code = match rejection {
			JsonRejection::MissingJsonContentType(_) => Code::UnsupportedMediaType,
			_ => Code::InvalidJson,
		};
		Err(ApiError::new(code, rejection.body_text()))
	}
}

/// A query-string extractor that answers an unreadable parameter with the envelope.
///
/// Thirty-one routes take a typed `Query<T>`, and until now a parameter the endpoint could
/// not read — `?start=soon` on a read that wants an integer, or a window with the `end`
/// left out — came back as axum's plain-text `400`: not JSON, no `code`, no `request_id`.
/// Those are the most-travelled routes in the API, so the hole mattered more than the
/// `Json` one. Handlers take `StrictQuery(params): StrictQuery<MyParams>` and the value
/// binds exactly as before; the message stays axum's, because it already names the
/// offending parameter and what was wrong with it.
///
/// Like axum's `Query`, this reads the request parts rather than the body: a text endpoint
/// such as the ILP one takes a query *and* a body, and a route may only have one extractor
/// that consumes the request.
#[derive(Debug)]
pub struct StrictQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for StrictQuery<T>
where
	T: DeserializeOwned,
	S: Send + Sync,
{
	type Rejection = ApiError;

	async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
		Query::<T>::from_request_parts(parts, state).await.map(|Query(value)| Self(value)).map_err(|rejection| ApiError::bad_request(rejection.body_text()))
	}
}

#[cfg(test)]
mod tests {
	use axum::{body::Body, http::StatusCode, response::Response};
	use serde::Deserialize;
	use serde_json::Value;

	/// Parse a response body as JSON, for the assertions that care about the wire shape.
	async fn response_json(response: Response) -> Value {
		let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.expect("the envelope fits");
		serde_json::from_slice(&bytes).expect("the body is JSON")
	}

	use super::*;

	/// Every code in the table, with the status and the wire name the plan's §4 table gives
	/// it. A new code is added to the plan's table and here together — that pairing is the
	/// contract, and `the_table_is_the_plan_table` is what enforces it.
	const TABLE: &[(Code, StatusCode, &str)] = &[(Code::BadRequest, StatusCode::BAD_REQUEST, "bad_request"), (Code::InvalidJson, StatusCode::BAD_REQUEST, "invalid_json"), (Code::InvalidAspectName, StatusCode::BAD_REQUEST, "invalid_aspect_name"), (Code::DecimalOutOfRange, StatusCode::BAD_REQUEST, "decimal_out_of_range"), (Code::NotFinite, StatusCode::BAD_REQUEST, "not_finite"), (Code::CredentialsInQuery, StatusCode::BAD_REQUEST, "credentials_in_query"), (Code::InvalidTag, StatusCode::BAD_REQUEST, "invalid_tag"), (Code::AmbiguousSeries, StatusCode::BAD_REQUEST, "ambiguous_series"), (Code::TagsUnsupported, StatusCode::BAD_REQUEST, "tags_unsupported"), (Code::SeriesLimit, StatusCode::BAD_REQUEST, "series_limit"), (Code::Unauthenticated, StatusCode::UNAUTHORIZED, "unauthenticated"), (Code::Forbidden, StatusCode::FORBIDDEN, "forbidden"), (Code::HostNotAllowed, StatusCode::FORBIDDEN, "host_not_allowed"), (Code::CrossOriginWrite, StatusCode::FORBIDDEN, "cross_origin_write"), (Code::ProxiedRequestRequiresAuth, StatusCode::FORBIDDEN, "proxied_request_requires_auth"), (Code::RouteNotFound, StatusCode::NOT_FOUND, "route_not_found"), (Code::AspectNotFound, StatusCode::NOT_FOUND, "aspect_not_found"), (Code::MethodNotAllowed, StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"), (Code::BodyTimeout, StatusCode::REQUEST_TIMEOUT, "body_timeout"), (Code::SchemaConflict, StatusCode::CONFLICT, "schema_conflict"), (Code::AlreadyExists, StatusCode::CONFLICT, "already_exists"), (Code::Busy, StatusCode::CONFLICT, "busy"), (Code::IdempotencyConflict, StatusCode::CONFLICT, "idempotency_conflict"), (Code::CursorStale, StatusCode::GONE, "cursor_stale"), (Code::PayloadTooLarge, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"), (Code::TooManyRows, StatusCode::PAYLOAD_TOO_LARGE, "too_many_rows"), (Code::UnsupportedMediaType, StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type"), (Code::ResultTooLarge, StatusCode::UNPROCESSABLE_ENTITY, "result_too_large"), (Code::ValueNotRepresentable, StatusCode::UNPROCESSABLE_ENTITY, "value_not_representable"), (Code::Internal, StatusCode::INTERNAL_SERVER_ERROR, "internal"), (Code::CommitFailed, StatusCode::INTERNAL_SERVER_ERROR, "commit_failed"), (Code::FrameUndecodable, StatusCode::INTERNAL_SERVER_ERROR, "frame_undecodable"), (Code::SeriesBindingMismatch, StatusCode::INTERNAL_SERVER_ERROR, "series_binding_mismatch"), (Code::Unconfigured, StatusCode::SERVICE_UNAVAILABLE, "unconfigured"), (Code::NotReady, StatusCode::SERVICE_UNAVAILABLE, "not_ready"), (Code::IncompatibleStore, StatusCode::SERVICE_UNAVAILABLE, "incompatible_store"), (Code::AuditUnavailable, StatusCode::SERVICE_UNAVAILABLE, "audit_unavailable"), (Code::Overloaded, StatusCode::SERVICE_UNAVAILABLE, "overloaded"), (Code::OutcomeUnknown, StatusCode::SERVICE_UNAVAILABLE, "outcome_unknown"), (Code::Poisoned, StatusCode::SERVICE_UNAVAILABLE, "poisoned"), (Code::QueryTimeout, StatusCode::GATEWAY_TIMEOUT, "query_timeout"), (Code::InsufficientStorage, StatusCode::INSUFFICIENT_STORAGE, "insufficient_storage")];

	async fn json_of(response: Response) -> Value {
		let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.expect("the envelope fits");
		serde_json::from_slice(&bytes).expect("the envelope is JSON")
	}

	/// The table is the plan's table: 42 codes, each with §4's status and §4's spelling.
	#[test]
	fn the_table_is_the_plan_table() {
		assert_eq!(TABLE.len(), 42, "§4 lists 42 codes; a new code goes into the plan's table and here together");
		for &(code, status, name) in TABLE {
			assert_eq!(code.status(), status, "`{name}` must be sent with {status}");
			assert_eq!(code.as_str(), name, "the wire name is the plan's spelling");
		}
		let mut names: Vec<&str> = TABLE.iter().map(|&(_, _, name)| name).collect();
		names.sort_unstable();
		names.dedup();
		assert_eq!(names.len(), TABLE.len(), "two codes share a wire name");
	}

	/// The synonyms C6 dropped stay dropped: a client that coded against the old vocabulary
	/// gets a code it can map, not a second name for a condition the table already has.
	#[test]
	fn the_dropped_synonyms_stay_dropped() {
		let names: Vec<&str> = TABLE.iter().map(|&(_, _, name)| name).collect();
		for dropped in ["invalid_request", "unauthorized", "store_unconfigured", "maintenance_busy", "limit_exceeded", "panic", "segment_invalid", "not_found", "too_large"] {
			assert!(!names.contains(&dropped), "`{dropped}` is a dropped synonym (C6) and must not come back");
		}
	}

	/// Every 5xx needs its own sentence, because the sentence is all a 5xx caller gets: two
	/// codes sharing one would leave the code as the only difference, and the sentence is
	/// what an operator reads off a terminal.
	#[test]
	fn every_server_error_has_its_own_sentence() {
		let mut seen: Vec<&str> = Vec::new();
		for &(code, status, name) in TABLE {
			if !status.is_server_error() {
				continue;
			}
			let sentence = code.redacted_message();
			assert!(!sentence.is_empty(), "`{name}` has no redacted sentence");
			assert!(!seen.contains(&sentence), "`{name}` shares its sentence with another code");
			seen.push(sentence);
			assert!(!sentence.contains('/') && !sentence.contains(':'), "`{name}`'s sentence names a path or subsystem: {sentence}");
		}
		assert_eq!(seen.len(), 13, "13 of the table's codes are 5xx");
	}

	#[tokio::test]
	async fn a_caller_error_keeps_its_message_and_gains_a_code() {
		let response = ApiError::new(Code::InvalidTag, "the tag `unit` may not be empty").with_details(serde_json::json!({ "tag": "unit" })).into_response();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
		let body = json_of(response).await;
		assert_eq!(body["error"], "the tag `unit` may not be empty");
		assert_eq!(body["code"], "invalid_tag");
		assert_eq!(body["details"]["tag"], "unit");
		assert!(body.get("limit").is_none(), "no limit was given, so the field is absent rather than null");
		assert!(body.get("committed").is_none(), "a non-write error says nothing about commit state");
	}

	#[tokio::test]
	async fn a_server_error_sends_a_sentence_and_keeps_the_cause_for_the_log() {
		let cause = "libSQL returned `no such table: main.segment_index` while opening /srv/weft/control.db";
		let err = ApiError::internal(cause);
		assert_eq!(err.message(), cause, "the cause is still reachable for logging and tests");
		let response = err.into_response();
		assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
		let body = json_of(response).await;
		assert_eq!(body["code"], "internal");
		let text = body["error"].as_str().unwrap();
		assert_eq!(text, "the server could not complete the request");
		assert!(!text.contains("libSQL") && !text.contains("srv"), "the cause leaked into the body: {text}");
	}

	#[tokio::test]
	async fn a_write_that_did_not_commit_says_the_store_is_unchanged() {
		let body = json_of(ApiError::commit_failed("the seal was rejected after the journal was fsynced").into_response()).await;
		assert_eq!(body["code"], "commit_failed");
		assert_eq!(body["committed"], false, "a retry is safe, and the body says so");
	}

	#[tokio::test]
	async fn a_cap_names_its_number_rather_than_burying_it_in_prose() {
		let body = json_of(ApiError::new(Code::ResultTooLarge, "the grid would hold 12000 points").with_limit(5000).into_response()).await;
		assert_eq!(body["code"], "result_too_large");
		assert_eq!(body["limit"], 5000);
		assert_eq!(body["error"], "the grid would hold 12000 points", "422 is a caller error, so its message is kept");
	}

	#[tokio::test]
	async fn retry_after_appears_only_when_the_handler_knew_a_wait() {
		let plain = ApiError::new(Code::Busy, "another maintenance run holds `price`").into_response();
		assert!(plain.headers().get(axum::http::header::RETRY_AFTER).is_none(), "an invented wait would be a lie");
		let told = ApiError::new(Code::Busy, "another maintenance run holds `price`").with_retry_after(30).into_response();
		assert_eq!(told.headers().get(axum::http::header::RETRY_AFTER).map(|value| value.to_str().unwrap()), Some("30"));
	}

	/// The marker is how `trace::request_span` finds the bodies to stamp with a `request_id`.
	/// It is consumed there and must never reach a client; asserting it here keeps both halves
	/// of the handshake described in one file.
	#[tokio::test]
	async fn every_envelope_is_marked_for_the_request_id_stamp() {
		let response = ApiError::new(Code::RouteNotFound, "no route serves POST /api/v1/nope").into_response();
		assert_eq!(response.headers().get(&ENVELOPE_MARKER).map(|value| value.to_str().unwrap()), Some("1"));
	}

	/// A JSON-typed route with the wrong `Content-Type` is a different complaint from a body
	/// that will not parse: the client sent the wrong kind of document.
	fn json_request(body: &str) -> Request {
		let mut request = Request::new(Body::from(body.to_string()));
		request.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"));
		request
	}

	#[tokio::test]
	async fn a_json_body_that_does_not_parse_comes_back_as_the_envelope() {
		let err = StrictJson::<Value>::from_request(json_request("{\"aspect\": price,}"), &()).await.expect_err("that body is not JSON");
		assert_eq!(err.code(), Code::InvalidJson);
		assert_eq!(err.code().status(), StatusCode::BAD_REQUEST);
		let body = json_of(err.into_response()).await;
		assert_eq!(body["code"], "invalid_json");
		assert!(!body["error"].as_str().unwrap().is_empty(), "the caller is told what would not parse");
	}

	#[tokio::test]
	async fn a_body_that_is_not_json_at_all_is_a_refusal_not_a_parse_failure() {
		let mut request = Request::new(Body::from("aspect=price"));
		request.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/x-www-form-urlencoded"));
		let err = StrictJson::<Value>::from_request(request, &()).await.expect_err("that body is not a JSON document");
		assert_eq!(err.code(), Code::UnsupportedMediaType);
		let body = json_of(err.into_response()).await;
		assert_eq!(body["code"], "unsupported_media_type");
	}

	/// The shape one read endpoint expects, used to exercise the query extractor.
	#[derive(Debug, Deserialize)]
	struct Window {
		start: i64,
		end: i64,
	}

	/// Bind the query of `uri` as a `Window`, the way a handler would.
	async fn bind_window(uri: &str) -> Result<Window, ApiError> {
		let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
		let (mut parts, _body) = request.into_parts();
		let StrictQuery(window) = StrictQuery::<Window>::from_request_parts(&mut parts, &()).await?;
		Ok(window)
	}

	#[tokio::test]
	async fn a_query_the_endpoint_cannot_read_becomes_the_envelope() {
		// `?start=soon` used to be axum's plain-text 400. The text is kept — it names the
		// parameter — but it arrives as JSON with a code now.
		let rejection = bind_window("/api/v1/storage/temperature/points?start=soon&end=2").await.expect_err("a word is not an integer");
		assert_eq!(rejection.code(), Code::BadRequest);
		let body = response_json(rejection.into_response()).await;
		assert_eq!(body["code"], "bad_request", "{body}");
		assert_eq!(body["error"], "Failed to deserialize query string: start: invalid digit found in string", "{body}");
	}

	#[tokio::test]
	async fn a_query_missing_a_required_parameter_is_the_same_refusal() {
		let rejection = bind_window("/api/v1/storage/temperature/points?start=1").await.expect_err("the window needs both bounds");
		assert_eq!(rejection.code(), Code::BadRequest);
		assert!(rejection.message().contains("end"), "the message names what is missing: {}", rejection.message());
	}

	#[tokio::test]
	async fn a_query_the_endpoint_can_read_still_binds() {
		let window = bind_window("/api/v1/storage/temperature/points?start=1&end=2").await.expect("a readable window");
		assert_eq!((window.start, window.end), (1, 2));
	}

	#[tokio::test]
	async fn a_well_formed_body_still_extracts() {
		let request = json_request("{\"aspect\":\"price\"}");
		let StrictJson(value) = StrictJson::<Value>::from_request(request, &()).await.expect("the body is fine");
		assert_eq!(value["aspect"], "price");
	}
}
