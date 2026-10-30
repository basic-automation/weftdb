//! API keys and Bearer authentication (1.0 plan **SEC-6**).
//!
//! SEC-3 built the sealed route registry and froze the vocabulary this
//! module enforces: eight [`Scope`]s, six [`Role`]s with cumulative grants,
//! and a policy table that says which scope each route needs. SEC-5 mounted
//! the keys file into the deployment and read its *presence* for the C8
//! plaintext gate. This module is the enforcement half: it defines the keys
//! file, loads it once at startup, and checks every request that reaches a
//! registered route against the table — through the registry's own lookup,
//! so the scope a key is checked against is the scope the route registered
//! with, never a second copy of the table.
//!
//! ## The keys file
//!
//! [`KEYS_FILE_ENV`] (`WEFT_KEYS_FILE`, the name SEC-5's gate already
//! reads) names a JSON document:
//!
//! ```json
//! {
//!   "version": 1,
//!   "keys": [
//!     { "id": "operator", "role": "admin", "key_hash": "sha256:<64 hex>" },
//!     { "id": "ingest-1", "role": "writer", "key_hash": "sha256:<64 hex>" }
//!   ]
//! }
//! ```
//!
//! The plan names the file and the tool but not the format (the queue's
//! recorded open question for this slice); this shape is the choice the
//! SEC-6 entry in `DECISIONS.md` records, flagged for merge review. Three
//! properties are not negotiable, whatever the format: the file holds
//! **hashes, never keys** (a mounted file that leaks is a list of ids and
//! roles, not a set of working credentials); every entry names one of the
//! six frozen roles (SEC-3 left "which key holds which role" to this
//! slice, and per-aspect grants are deferred past 1.0 by plan §10, so a
//! role applies to every aspect alike); and a malformed file — unknown
//! field, unknown role, duplicate id, a hash shared by two ids, an empty
//! key list — is a startup error, never a partially enforced table.
//! [`generate_api_key`] plus the `weft-server keygen` command
//! ([`crate::keygen`]) mint a key and its `key_hash` entry; the plaintext
//! key exists only in that command's output. Key *reload* without a
//! restart is SEC-7, not this slice: the file is read once, at startup.
//!
//! ## Modes ([`AUTH_ENV`], `WEFT_AUTH`)
//!
//! - `disabled` — authentication explicitly off (SEC-5's value: the
//!   gateway deployment's opt-in, decision 7). Wins over a mounted keys
//!   file, exactly as it does in the plaintext gate.
//! - `keys` — authentication on, and a keys file is required: naming the
//!   mode without mounting the file is a startup error.
//! - unset — authentication on when a keys file is mounted, off when none
//!   is. A server with no keys file behaves exactly as it did before this
//!   slice (the loopback guard is its protection); the full exposure
//!   matrix that refuses some of those binds is SEC-8's.
//!
//! ## What a request must carry
//!
//! `Authorization: Bearer <key>` — the only place a credential may travel.
//! A credential in the query string is refused `400 credentials_in_query`
//! (the code has been in the §4 table since X-ERR; this slice is its first
//! producer), because query strings land in logs and proxies. The check
//! runs as a layer in [`crate::app_with_state`], after the registry's
//! fail-closed gate has matched the route, and it decides from the same
//! [`RouteTable`] the gate sealed:
//!
//! - no `Authorization` header — the caller is the anonymous principal
//!   (SEC-3's [`Role::Anonymous`], probes only). A route asking for the
//!   probe scope serves; anything else is refused `401 unauthenticated`
//!   with a `WWW-Authenticate: Bearer` header.
//! - a malformed header, or a key the file does not hold — the same `401`,
//!   on every route including the probes: a caller that *tried* to
//!   authenticate and failed is told so, not silently demoted to
//!   anonymous.
//! - a key whose role lacks the route's scope — `403 forbidden`, naming
//!   the key id, its role and the scope the route needs (ids and roles
//!   are not secrets; the key is).
//! - a key whose role holds the scope — the request passes, carrying its
//!   [`Principal`] in its extensions for the layers that follow (SEC-9's
//!   audit reads it there).
//!
//! A request the registry itself refuses (an unregistered path, a wrong
//! method) is left to the gate and the fallback: the auth layer defers
//! whenever its (method, path) lookup finds no entry, so a `404`/`405`
//! keeps its shape whether or not auth is on.
//!
//! ## The key namespace (plan §4, C10)
//!
//! C10 fixes the *idempotency* key namespace the ingest wiring will use:
//! per aspect, and shared by all principals. Nothing in this module
//! partitions anything by principal — [`KeyStore::authenticate`] answers
//! "who is this key" and the registry answers "what may that role do", on
//! every aspect alike — and the ingest ledger D-S13 built is keyed by
//! `(aspect, key)` with no principal component. When D-S14 (W5) puts
//! `Idempotency-Key` handling behind this gate, a replay by a second
//! principal therefore meets the first principal's ledger row, which is
//! exactly the sharing C10 names; adding the caller's key id to that
//! namespace would silently break it.

use std::{
	fs, path::{Path, PathBuf}, sync::{Arc, OnceLock}
};

use axum::{
	extract::{MatchedPath, State}, http::{header, HeaderValue, Request}, middleware::Next, response::{IntoResponse, Response}
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
	error::{ApiError, Code}, routes::{Role, RouteTable, Scope}, state::AppState, tls::{AUTH_DISABLED, AUTH_ENV, KEYS_FILE_ENV}
};

/// The [`AUTH_ENV`] value that turns authentication on and requires a
/// mounted keys file. (The value that turns it off, `disabled`, is
/// SEC-5's [`AUTH_DISABLED`].)
pub const AUTH_KEYS_MODE: &str = "keys";

/// The prefix every key [`generate_api_key`] mints carries, so a key is
/// recognisable in a log line or a password manager as a WeftDB key. The
/// prefix is part of the hashed text: a key authenticates exactly as
/// printed, prefix included.
pub const TOKEN_PREFIX: &str = "weft_";

/// The only keys-file version this build reads. A later format change
/// bumps it and the loader says so, rather than guessing at a document
/// written for another shape.
const KEYS_FILE_VERSION: u32 = 1;

/// The challenge a `401` carries (the §4 table pairs `unauthenticated`
/// with `WWW-Authenticate`).
const WWW_AUTHENTICATE_VALUE: &str = "Bearer realm=\"weftdb\"";

/// Query parameter names that are credentials, refused when auth is on:
/// a key belongs in the `Authorization` header, not in a string that
/// proxies and access logs record.
const CREDENTIAL_QUERY_PARAMS: &[&str] = &["api_key", "apikey", "access_token", "auth_token"];

/// Who a request authenticated as: the key's id and the role the keys
/// file grants it. Inserted into the request's extensions by
/// [`enforce`] once a key checks out, for the layers that follow
/// (SEC-9's audit). Ids and roles are not secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
	/// The key's `id` in the keys file.
	pub id: String,
	/// The role the keys file grants this key.
	pub role: Role,
}

/// One keys-file entry, after validation: who the key is, what it may
/// do, and the SHA-256 of the key itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyEntry {
	id: String,
	role: Role,
	digest: [u8; 32],
}

/// The loaded keys file: the principals this server accepts, and where
/// they came from. Read once at startup; SEC-7 owns reloading it.
#[derive(Debug, Clone)]
pub struct KeyStore {
	entries: Vec<KeyEntry>,
	path: Option<PathBuf>,
}

/// The keys file as it sits on disk. Unknown fields are refused: a
/// typo'd field name (`roles`, `hash`) silently ignored would mint a
/// key with whatever the defaults are, which is the configuration
/// failure the settings registry exists to catch, one file over.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeysFile {
	version: u32,
	keys: Vec<KeysFileEntry>,
}

/// One entry of [`KeysFile`], exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeysFileEntry {
	id: String,
	role: String,
	key_hash: String,
}

impl KeyStore {
	/// Parse a keys file's text. See the module docs for the format.
	///
	/// # Errors
	///
	/// A message naming what is wrong: unparseable JSON, an unknown
	/// field, a version this build does not read, an empty key list, a
	/// blank or duplicate id, a role outside the frozen six (or
	/// `anonymous` — a key that grants nothing beyond the public probes
	/// is a configuration mistake, not a principal), or a `key_hash`
	/// that is not `sha256:` plus 64 hex digits, or one shared by two
	/// ids (which principal a key names must never be ambiguous).
	pub fn parse(text: &str) -> Result<Self, String> {
		let file: KeysFile = serde_json::from_str(text).map_err(|e| format!("the keys file is not a keys document: {e}"))?;
		if file.version != KEYS_FILE_VERSION {
			return Err(format!("the keys file is version {}, and this build reads version {KEYS_FILE_VERSION}", file.version));
		}
		if file.keys.is_empty() {
			return Err("the keys file holds no keys: authentication would refuse every caller. Remove the file (and unset WEFT_AUTH) to run without authentication, or add a key with `weft-server keygen`".to_string());
		}
		let mut entries: Vec<KeyEntry> = Vec::with_capacity(file.keys.len());
		for entry in file.keys {
			if entry.id.trim().is_empty() || entry.id.chars().any(char::is_whitespace) {
				return Err(format!("a key's id must be one non-blank word, not {:?}", entry.id));
			}
			if entries.iter().any(|known: &KeyEntry| known.id == entry.id) {
				return Err(format!("the keys file names the id {:?} twice; a key id must name one principal", entry.id));
			}
			let role = role_from_name(&entry.role).ok_or_else(|| format!("key {:?} holds role {:?}, which is not one of the frozen roles ({})", entry.id, entry.role, Role::ALL.iter().map(|role| role.as_str()).collect::<Vec<_>>().join(", ")))?;
			if role == Role::Anonymous {
				return Err(format!("key {:?} holds the anonymous role, which grants nothing beyond the public probes: no key is needed for those, and a key that grants nothing is a configuration mistake", entry.id));
			}
			let digest = parse_key_hash(&entry.key_hash).ok_or_else(|| format!("key {:?} has a key_hash that is not `sha256:` plus 64 hex digits; `weft-server keygen` prints the entry to paste", entry.id))?;
			if entries.iter().any(|known: &KeyEntry| known.digest == digest) {
				return Err(format!("key {:?} shares its key_hash with an earlier entry; one key must name one principal", entry.id));
			}
			entries.push(KeyEntry { id: entry.id, role, digest });
		}
		Ok(Self { entries, path: None })
	}

	/// Load the keys file at `path`.
	///
	/// # Errors
	///
	/// A file that cannot be read (the message names the variable and
	/// the path), or whatever [`parse`](Self::parse) rejects.
	pub fn load(path: &Path) -> Result<Self, String> {
		let text = fs::read_to_string(path).map_err(|e| format!("{KEYS_FILE_ENV}={}: cannot read the keys file: {e}", path.display()))?;
		let mut store = Self::parse(&text).map_err(|e| format!("{KEYS_FILE_ENV}={}: {e}", path.display()))?;
		store.path = Some(path.to_path_buf());
		Ok(store)
	}

	/// The path the store was loaded from, when it was loaded from one.
	#[must_use]
	pub fn path(&self) -> Option<&Path> {
		self.path.as_deref()
	}

	/// How many keys the file holds.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.entries.len()
	}

	/// Whether the file holds no keys. [`parse`](Self::parse) refuses
	/// that, so a loaded store is never empty; the method exists so the
	/// invariant is checkable rather than assumed.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	/// Whether a key with `id` is in the file (`weft-server keygen`
	/// asks before appending, so an id is never quietly redefined).
	#[must_use]
	pub fn contains_id(&self, id: &str) -> bool {
		self.entries.iter().any(|entry| entry.id == id)
	}

	/// Who `token` is, if the file holds it. The token is hashed and
	/// the digest compared against **every** entry in constant time
	/// per entry, with no early exit on a match: the work a lookup does
	/// must not say which entry matched, or how close a wrong key came.
	#[must_use]
	pub fn authenticate(&self, token: &str) -> Option<Principal> {
		let digest = digest_of(token);
		let mut found: Option<&KeyEntry> = None;
		for entry in &self.entries {
			if constant_time_eq(&digest, &entry.digest) {
				found = Some(entry);
			}
		}
		found.map(|entry| Principal { id: entry.id.clone(), role: entry.role })
	}
}

/// The role a frozen role name denotes, if it is one of the six.
#[must_use]
pub fn role_from_name(name: &str) -> Option<Role> {
	Role::ALL.iter().copied().find(|role| role.as_str() == name.trim())
}

/// A freshly minted API key and the `key_hash` a keys-file entry for it
/// carries. The token is shown once, by the tool that minted it; only
/// the hash is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedKey {
	/// The key itself, prefix included: `weft_` plus 64 hex digits.
	pub token: String,
	/// `sha256:` plus the hex digest of the token — the entry's
	/// `key_hash` field.
	pub key_hash: String,
}

/// Mint a new API key: 32 bytes from the ring provider's secure random
/// (the same provider SEC-5's TLS listener is built on — no new entropy
/// source, and no process-global provider is installed or consulted),
/// hex-encoded behind [`TOKEN_PREFIX`].
///
/// # Errors
///
/// The provider refusing to fill the buffer, which is reported rather
/// than papered over with weaker randomness.
pub fn generate_api_key() -> Result<GeneratedKey, String> {
	let provider = rustls::crypto::ring::default_provider();
	let mut bytes = [0u8; 32];
	provider.secure_random.fill(&mut bytes).map_err(|_| "the ring provider could not produce randomness for a new key".to_string())?;
	let token = format!("{TOKEN_PREFIX}{}", hex_encode(&bytes));
	Ok(GeneratedKey { key_hash: format_key_hash(&token), token })
}

/// The SHA-256 digest of a presented key — the only form in which a key
/// is ever stored or compared.
fn digest_of(token: &str) -> [u8; 32] {
	let digest = Sha256::digest(token.as_bytes());
	let mut out = [0u8; 32];
	out.copy_from_slice(&digest);
	out
}

/// The `key_hash` field value for a key: `sha256:` plus its hex digest.
#[must_use]
pub fn format_key_hash(token: &str) -> String {
	format!("sha256:{}", hex_encode(&digest_of(token)))
}

/// A `key_hash` field's digest, if the field is exactly `sha256:` plus
/// 64 hex digits.
fn parse_key_hash(field: &str) -> Option<[u8; 32]> {
	let hex = field.strip_prefix("sha256:")?;
	if hex.len() != 64 {
		return None;
	}
	let mut digest = [0u8; 32];
	for (index, slot) in digest.iter_mut().enumerate() {
		*slot = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
	}
	Some(digest)
}

/// Lowercase hex for `bytes`.
fn hex_encode(bytes: &[u8]) -> String {
	use std::fmt::Write as _;
	let mut out = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		let _ = write!(out, "{byte:02x}");
	}
	out
}

/// Whether two digests are equal, without the comparison's timing
/// saying where they first differed.
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
	let mut difference = 0u8;
	for (left, right) in a.iter().zip(b.iter()) {
		difference |= left ^ right;
	}
	difference == 0
}

/// How authentication is configured for this process: off, or on with
/// the loaded keys. Carried in [`AppState`]; the binary resolves it
/// once at startup ([`from_env`](Self::from_env)).
#[derive(Debug, Clone, Default)]
pub enum AuthConfig {
	/// Authentication off: no keys file mounted, or
	/// `WEFT_AUTH=disabled` explicitly. Every request is the anonymous
	/// principal, exactly as before SEC-6.
	#[default]
	Off,
	/// Authentication on: every non-probe route needs a key from this
	/// store whose role holds the route's scope.
	Keys(Arc<KeyStore>),
}

impl AuthConfig {
	/// Authentication off. (The `const` constructor
	/// [`AppState`]'s own const constructors need.)
	#[must_use]
	pub const fn off() -> Self {
		Self::Off
	}

	/// Authentication on, enforcing `store`.
	#[must_use]
	pub fn with_keys(store: KeyStore) -> Self {
		Self::Keys(Arc::new(store))
	}

	/// Whether requests are authenticated at all.
	#[must_use]
	pub const fn is_enabled(&self) -> bool {
		matches!(self, Self::Keys(_))
	}

	/// The key store in force, when authentication is on.
	#[must_use]
	pub fn key_store(&self) -> Option<&KeyStore> {
		match self {
			Self::Off => None,
			Self::Keys(store) => Some(store),
		}
	}

	/// The configuration two environment values describe (`WEFT_AUTH`
	/// and `WEFT_KEYS_FILE`). See the module docs for the modes.
	///
	/// # Errors
	///
	/// An unrecognised `WEFT_AUTH` value (the opt-out and the opt-in
	/// are both only ever explicit, decision 7 — a typo must not read
	/// as either), `keys` mode with no keys file named, or a keys file
	/// that cannot be loaded.
	pub fn from_env_values(auth_mode: Option<&str>, keys_file: Option<&str>) -> Result<Self, String> {
		let mode = auth_mode.map(str::trim).filter(|value| !value.is_empty());
		if mode.is_some_and(|value| value.eq_ignore_ascii_case(AUTH_DISABLED)) {
			return Ok(Self::Off);
		}
		if let Some(value) = mode {
			if !value.eq_ignore_ascii_case(AUTH_KEYS_MODE) {
				return Err(format!("{AUTH_ENV}={value} is not an authentication mode this build knows: `{AUTH_KEYS_MODE}` turns Bearer authentication on, `{AUTH_DISABLED}` turns it off explicitly, and unset lets a mounted keys file decide"));
			}
			let Some(path) = keys_file else {
				return Err(format!("{AUTH_ENV}={AUTH_KEYS_MODE} requires a keys file, and {KEYS_FILE_ENV} is unset: mount one (the Docker image does), or mint one with `weft-server keygen --keys-file <path>`"));
			};
			return Ok(Self::with_keys(KeyStore::load(Path::new(path))?));
		}
		match keys_file {
			Some(path) => Ok(Self::with_keys(KeyStore::load(Path::new(path))?)),
			None => Ok(Self::Off),
		}
	}

	/// The configuration the process environment describes; see
	/// [`from_env_values`](Self::from_env_values).
	///
	/// # Errors
	///
	/// Whatever [`from_env_values`](Self::from_env_values) rejects.
	pub fn from_env() -> Result<Self, String> {
		Self::from_env_values(std::env::var(AUTH_ENV).ok().as_deref(), std::env::var(KEYS_FILE_ENV).ok().as_deref())
	}
}

/// The sealed policy table, built once: the registry is frozen, so the
/// table the gate sealed and the table this layer consults are the same
/// values, and building it per request would only rebuild the router's
/// handler set to throw it away.
fn policy_table() -> &'static RouteTable {
	static TABLE: OnceLock<Arc<RouteTable>> = OnceLock::new();
	let table: &'static Arc<RouteTable> = TABLE.get_or_init(|| crate::routes::build_registry().table());
	&**table
}

/// What the `Authorization` header carried, before any role check.
enum Presented {
	/// No header at all: the anonymous principal.
	Absent,
	/// A header that is malformed, or a key the file does not hold:
	/// one refusal for both, so a response never says which.
	Refused,
	/// A key the file holds.
	Principal(Principal),
}

/// Read the `Authorization` header of a request against `store`.
fn presented(headers: &axum::http::HeaderMap, store: &KeyStore) -> Presented {
	let mut values = headers.get_all(header::AUTHORIZATION).iter();
	let Some(value) = values.next() else {
		return Presented::Absent;
	};
	if values.next().is_some() {
		return Presented::Refused;
	}
	let Ok(value) = value.to_str() else {
		return Presented::Refused;
	};
	let mut parts = value.splitn(2, ' ');
	let (Some(scheme), Some(token)) = (parts.next(), parts.next()) else {
		return Presented::Refused;
	};
	if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() || token.trim() != token || token.contains(char::is_whitespace) {
		return Presented::Refused;
	}
	store.authenticate(token).map_or(Presented::Refused, Presented::Principal)
}

/// Whether the request's query string carries a credential (see the
/// module docs). Only the parameter *names* are read; values are never
/// logged or echoed.
fn query_carries_a_credential(query: Option<&str>) -> bool {
	let Some(query) = query else {
		return false;
	};
	query.split('&').any(|pair| {
		let name = pair.split('=').next().unwrap_or_default();
		CREDENTIAL_QUERY_PARAMS.iter().any(|known| name.eq_ignore_ascii_case(known))
	})
}

/// The `401 unauthenticated` refusal, with the `WWW-Authenticate`
/// challenge the §4 table pairs with the code.
fn unauthenticated(message: String) -> Response {
	let mut response = ApiError::new(Code::Unauthenticated, message).into_response();
	response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(WWW_AUTHENTICATE_VALUE));
	response
}

/// axum middleware: enforce the keys file against the sealed registry,
/// as described in the module docs. Installed by
/// [`crate::app_with_state`] from the state's [`AuthConfig`]; with
/// authentication off it is a pass-through, so a keyless server serves
/// exactly the router it served before SEC-6.
pub async fn enforce(State(state): State<AppState>, mut request: Request<axum::body::Body>, next: Next) -> Response {
	let Some(store) = state.auth().key_store() else {
		return next.run(request).await;
	};
	if query_carries_a_credential(request.uri().query()) {
		return ApiError::new(Code::CredentialsInQuery, "credentials belong in the `Authorization: Bearer <key>` header, not in the query string, where they are recorded by proxies and logs").into_response();
	}
	let Some(matched) = request.extensions().get::<MatchedPath>().map(|matched| matched.as_str().to_string()) else {
		// No route matched: the registry's fallback refuses it, and a
		// 404's shape does not depend on who asked.
		return next.run(request).await;
	};
	let Some(entry) = policy_table().lookup(request.method(), &matched) else {
		// A matched path under another method: the gate's 405, again
		// not this layer's answer to give.
		return next.run(request).await;
	};
	let scope = entry.spec.scope;
	match presented(request.headers(), store) {
		Presented::Absent if scope == Scope::Probe => next.run(request).await,
		Presented::Absent => unauthenticated(format!("this route needs the `{}` scope, and the request carries no key: send `Authorization: Bearer <key>` with a key whose role grants it", scope.as_str())),
		Presented::Refused => unauthenticated("the request's credentials are not a key this server holds: send `Authorization: Bearer <key>` with the key exactly as `weft-server keygen` printed it".to_string()),
		Presented::Principal(principal) => {
			if principal.role.allows(scope) {
				request.extensions_mut().insert(principal);
				next.run(request).await
			} else {
				ApiError::new(Code::Forbidden, format!("key `{}` holds the {} role, which does not grant the `{}` scope this route needs", principal.id, principal.role.as_str(), scope.as_str())).into_response()
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tower::ServiceExt;

	use super::*;
	use crate::AppState;

	const READER_TOKEN: &str = "weft_reader_token_for_tests";
	const WRITER_TOKEN: &str = "weft_writer_token_for_tests";
	const ADMIN_TOKEN: &str = "weft_admin_token_for_tests";

	fn keys_json() -> String {
		format!("{{\n  \"version\": 1,\n  \"keys\": [\n    {{ \"id\": \"reader-1\", \"role\": \"reader\", \"key_hash\": \"{}\" }},\n    {{ \"id\": \"ingest-1\", \"role\": \"writer\", \"key_hash\": \"{}\" }},\n    {{ \"id\": \"operator\", \"role\": \"admin\", \"key_hash\": \"{}\" }}\n  ]\n}}", format_key_hash(READER_TOKEN), format_key_hash(WRITER_TOKEN), format_key_hash(ADMIN_TOKEN),)
	}

	fn store() -> KeyStore {
		KeyStore::parse(&keys_json()).expect("the test keys file parses")
	}

	fn router() -> axum::Router {
		crate::app_with_state(AppState::new().with_auth(AuthConfig::with_keys(store())))
	}

	async fn send(router: axum::Router, method: &str, uri: &str, authorization: Option<&str>) -> (StatusCode, Option<String>, serde_json::Value) {
		let mut builder = Request::builder().method(method).uri(uri);
		if let Some(value) = authorization {
			builder = builder.header(header::AUTHORIZATION, value);
		}
		let response = router.oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let challenge = response.headers().get(header::WWW_AUTHENTICATE).map(|value| value.to_str().unwrap().to_string());
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, challenge, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
	}

	fn bearer(token: &str) -> String {
		format!("Bearer {token}")
	}

	/// A well-formed ingest request to `uri` under `token`: a JSON body
	/// the handler's extractor accepts, so the answer a caller gets is
	/// the store's (503 here, no store configured), not the extractor's.
	async fn send_ingest(router: axum::Router, uri: &str, token: &str) -> StatusCode {
		let body = serde_json::json!({ "points": [{ "timestamp": 0, "value": "1.0" }] });
		let request = Request::builder().method("POST").uri(uri).header(header::AUTHORIZATION, bearer(token)).header(header::CONTENT_TYPE, "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap();
		router.oneshot(request).await.unwrap().status()
	}

	#[test]
	fn a_keys_file_parses_and_authenticates_by_role() {
		let store = store();
		assert_eq!(store.len(), 3);
		assert!(!store.is_empty());
		assert!(store.contains_id("operator"));
		assert_eq!(store.authenticate(READER_TOKEN), Some(Principal { id: "reader-1".to_string(), role: Role::Reader }));
		assert_eq!(store.authenticate(WRITER_TOKEN).map(|p| p.role), Some(Role::Writer));
		assert_eq!(store.authenticate(ADMIN_TOKEN).map(|p| p.role), Some(Role::Admin));
		assert_eq!(store.authenticate("weft_no_such_key"), None);
		assert_eq!(store.authenticate(""), None);
	}

	#[test]
	fn a_malformed_keys_file_is_refused_with_the_reason() {
		for (text, needle) in [("not json".to_string(), "not a keys document"), ("{\"version\": 2, \"keys\": []}".to_string(), "version 2"), ("{\"version\": 1, \"keys\": []}".to_string(), "holds no keys"), (keys_json().replace("\"reader\"", "\"superuser\""), "not one of the frozen roles"), (keys_json().replace("\"reader\"", "\"anonymous\""), "anonymous role"), (keys_json().replace(&format_key_hash(READER_TOKEN), "sha256:not-hex"), "not `sha256:`"), (keys_json().replace("\"reader-1\"", "\"operator\""), "twice"), (format!("{{\"version\": 1, \"keys\": [{{\"id\": \"a\", \"role\": \"reader\", \"key_hash\": \"{}\"}}, {{\"id\": \"b\", \"role\": \"admin\", \"key_hash\": \"{}\"}}]}}", format_key_hash(READER_TOKEN), format_key_hash(READER_TOKEN)), "shares its key_hash"), (keys_json().replace("\"role\": \"reader\"", "\"roles\": \"reader\""), "unknown field")] {
			let err = KeyStore::parse(&text).expect_err("refused");
			assert!(err.contains(needle), "{err} should name {needle:?}");
		}
	}

	#[test]
	fn the_modes_follow_the_module_docs() {
		// Unset with no keys file: off, as before SEC-6.
		assert!(!AuthConfig::from_env_values(None, None).expect("off").is_enabled());
		// `disabled` wins, even over a mounted file.
		assert!(!AuthConfig::from_env_values(Some(" Disabled "), None).expect("off").is_enabled());
		// `keys` without a file is a startup error, not silent off.
		let err = AuthConfig::from_env_values(Some("keys"), None).expect_err("refused");
		assert!(err.contains(KEYS_FILE_ENV), "{err}");
		// An unknown mode is refused rather than read as either.
		assert!(AuthConfig::from_env_values(Some("token"), None).is_err());
		// Unset with a mounted file: on.
		let dir = tempfile::tempdir().expect("tempdir");
		let path = dir.path().join("keys.json");
		fs::write(&path, keys_json()).expect("write keys");
		let config = AuthConfig::from_env_values(None, path.to_str()).expect("on");
		assert!(config.is_enabled());
		assert_eq!(config.key_store().map(KeyStore::len), Some(3));
		assert_eq!(config.key_store().and_then(KeyStore::path), Some(path.as_path()));
		// A mounted file that is not a keys file refuses startup.
		fs::write(&path, "{}").expect("write non-keys");
		assert!(AuthConfig::from_env_values(None, path.to_str()).is_err());
	}

	#[test]
	fn a_generated_key_authenticates_against_its_hash() {
		let generated = generate_api_key().expect("mint");
		assert!(generated.token.starts_with(TOKEN_PREFIX));
		assert_eq!(generated.token.len(), TOKEN_PREFIX.len() + 64);
		assert_eq!(generated.key_hash, format_key_hash(&generated.token));
		let text = format!("{{\"version\": 1, \"keys\": [{{\"id\": \"minted\", \"role\": \"maintainer\", \"key_hash\": \"{}\"}}]}}", generated.key_hash);
		let store = KeyStore::parse(&text).expect("parses");
		assert_eq!(store.authenticate(&generated.token), Some(Principal { id: "minted".to_string(), role: Role::Maintainer }));
		let other = generate_api_key().expect("mint another");
		assert_ne!(other.token, generated.token);
		assert_eq!(store.authenticate(&other.token), None);
	}

	/// Probes are the anonymous role's one scope: an orchestrator with
	/// no key still learns the process is up, and that is all it learns.
	#[tokio::test]
	async fn probes_serve_without_a_key_and_nothing_else_does() {
		let (status, _, _) = send(router(), "GET", "/health", None).await;
		assert_eq!(status, StatusCode::OK);
		let (status, challenge, body) = send(router(), "GET", "/api/v1/storage/stats", None).await;
		assert_eq!(status, StatusCode::UNAUTHORIZED);
		assert_eq!(challenge.as_deref(), Some(WWW_AUTHENTICATE_VALUE));
		assert_eq!(body["code"], "unauthenticated");
	}

	/// A key that tried and failed is a 401 even on a probe: silently
	/// serving it as anonymous would hide a broken credential.
	#[tokio::test]
	async fn a_bad_key_is_refused_even_on_a_probe() {
		for header in [bearer("weft_no_such_key"), "Bearer".to_string(), "Basic dXNlcjpwYXNz".to_string(), format!("Bearer  {READER_TOKEN}")] {
			let (status, _, body) = send(router(), "GET", "/health", Some(&header)).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{header:?}: {body}");
			assert_eq!(body["code"], "unauthenticated");
		}
		let (status, _, _) = send(router(), "GET", "/health", Some(&bearer(READER_TOKEN))).await;
		assert_eq!(status, StatusCode::OK, "a real key serves a probe");
		// The scheme is case-insensitive, as HTTP authentication schemes are.
		let (status, _, _) = send(router(), "GET", "/health", Some(&format!("bearer {READER_TOKEN}"))).await;
		assert_eq!(status, StatusCode::OK);
	}

	/// The role check is the registry's scope table, route by route:
	/// a reader reads (and gets the store's own 503, having passed
	/// auth), cannot see metrics, and cannot ingest; a writer ingests;
	/// an admin reaches the metrics surface.
	#[tokio::test]
	async fn roles_are_checked_against_the_route_scope() {
		// Reader on a read route: past auth, the handler's own answer.
		let (status, _, body) = send(router(), "GET", "/api/v1/storage/stats", Some(&bearer(READER_TOKEN))).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
		assert_eq!(body["code"], "unconfigured", "no store is configured; auth is not what refused this");
		// Reader on the metrics scope: 403, naming the key and the scope.
		let (status, _, body) = send(router(), "GET", "/metrics", Some(&bearer(READER_TOKEN))).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
		assert_eq!(body["code"], "forbidden");
		assert!(body["error"].as_str().unwrap().contains("reader-1"), "{body}");
		assert!(body["error"].as_str().unwrap().contains("metrics"), "{body}");
		// Reader cannot ingest; writer can (and meets the same 503).
		let (status, _, _) = send(router(), "POST", "/api/v1/storage/price/points", Some(&bearer(READER_TOKEN))).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
		assert_eq!(send_ingest(router(), "/api/v1/storage/price/points", WRITER_TOKEN).await, StatusCode::SERVICE_UNAVAILABLE);
		// The key namespace is shared across aspects (C10): the same
		// writer key ingests into a second aspect with no second grant.
		assert_eq!(send_ingest(router(), "/api/v1/storage/temperature/points", WRITER_TOKEN).await, StatusCode::SERVICE_UNAVAILABLE);
		// Admin reaches metrics.
		let (status, _, _) = send(router(), "GET", "/metrics", Some(&bearer(ADMIN_TOKEN))).await;
		assert_eq!(status, StatusCode::OK);
	}

	/// A credential in the query string is refused even with a valid
	/// key in the header: the refusal is about where it travelled.
	#[tokio::test]
	async fn credentials_in_the_query_string_are_refused() {
		let (status, _, body) = send(router(), "GET", "/api/v1/storage/stats?api_key=weft_x", Some(&bearer(ADMIN_TOKEN))).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert_eq!(body["code"], "credentials_in_query");
		assert!(query_carries_a_credential(Some("start=1&ACCESS_TOKEN=x")));
		assert!(!query_carries_a_credential(Some("start=1&end=2")));
		assert!(!query_carries_a_credential(None));
	}

	/// The registry's own refusals keep their shape with auth on: auth
	/// defers to the gate for a wrong method and to the fallback for an
	/// unregistered path, keyed or not.
	#[tokio::test]
	async fn registry_refusals_are_not_auth_refusals() {
		let (status, _, body) = send(router(), "GET", "/nope", None).await;
		assert_eq!(status, StatusCode::NOT_FOUND);
		assert_eq!(body["code"], "route_not_found");
		let (status, _, body) = send(router(), "DELETE", "/health", None).await;
		assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
		assert_eq!(body["code"], "method_not_allowed");
	}

	/// With no keys configured the layer is a pass-through: the router
	/// serves as it did before SEC-6.
	#[tokio::test]
	async fn auth_off_serves_as_before() {
		let open = crate::app_with_state(AppState::new());
		let (status, _, _) = send(open, "GET", "/metrics", None).await;
		assert_eq!(status, StatusCode::OK);
	}
}
