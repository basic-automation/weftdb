//! SEC-13: security acceptance for the whole SEC-3…SEC-11 surface.
//!
//! The per-slice suites pin each control in isolation — SEC-3's registry
//! and role table, SEC-5's TLS listener, SEC-6's keys file, SEC-7's
//! reload, SEC-8's exposure matrix and auth-off guards, SEC-9's audit
//! log, SEC-10's credential hygiene, SEC-11's traversal regressions.
//! This suite is the acceptance pass the 1.0 plan ends the security
//! chain with (§6 W7, §7): the controls composed, the way a deployment
//! actually runs them, against the two ROADMAP gates SEC-13 ticks —
//! TLS on `weft-server`, and API-key/token auth + basic RBAC + audit
//! log.
//!
//! Two levels, mirroring the gates:
//!
//! - in-process, the full role × scope matrix over the real router
//!   (SEC-3/SEC-6), credential placement (SEC-6/SEC-10), live key
//!   reload (SEC-7), the audit trail a mixed run leaves (SEC-9), and
//!   traversal names meeting the gate (SEC-11);
//! - the real binary, TLS + mounted keys file + store + audit file on
//!   disk: the matrix's decisive cells over the wire, plaintext refused
//!   on the TLS port, SIGHUP reload with no restart (SEC-7), the audit
//!   file the run leaves behind (SEC-9), and the auth-off loopback
//!   guards — proxied requests and foreign hosts refused, the explicit
//!   `WEFT_AUTH=disabled` gateway mode serving them (SEC-8).
//!
//! Nothing here re-pins a slice's internals; where a behaviour is a
//! slice's own (the exposure matrix's startup refusals, the traversal
//! spellings, the audit file's fail-closed refusal), its suite owns it
//! and this file only checks the composed result.

use std::{
	path::{Path, PathBuf}, sync::Arc, time::Duration
};

use axum::{
	body::Body, http::{header, HeaderMap, Request, StatusCode}
};
use tower::ServiceExt as _;
use weft_server::{app_with_state, AppState, AuditLog, AuthConfig, KeyStore};
use weftdb::SegmentStore;

// --- Principals -------------------------------------------------------------

const READER_KEY: &str = "sec13-reader-key";
const WRITER_KEY: &str = "sec13-writer-key";
const DECLARER_KEY: &str = "sec13-declarer-key";
const MAINTAINER_KEY: &str = "sec13-maintainer-key";
const ADMIN_KEY: &str = "sec13-admin-key";
const UNKNOWN_KEY: &str = "sec13-no-such-key";

/// A keys document in the SEC-6 format holding one key per role that
/// can hold one (anonymous holds none — it is the absence of a key).
fn keys_document(entries: &[(&str, &str, &str)]) -> String {
	let keys: Vec<String> = entries.iter().map(|(id, role, key)| format!("{{\"id\": \"{id}\", \"role\": \"{role}\", \"key_hash\": \"{}\"}}", weft_server::auth::format_key_hash(key))).collect();
	format!("{{\"version\": 1, \"keys\": [{}]}}", keys.join(", "))
}

fn full_keys_document() -> String {
	keys_document(&[("reader-1", "reader", READER_KEY), ("writer-1", "writer", WRITER_KEY), ("declarer-1", "declarer", DECLARER_KEY), ("maintainer-1", "maintainer", MAINTAINER_KEY), ("operator", "admin", ADMIN_KEY)])
}

// --- In-process harness -----------------------------------------------------

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value, HeaderMap) {
	let response = router.oneshot(request).await.unwrap();
	let status = response.status();
	let headers = response.headers().clone();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null), headers)
}

fn request(method: &str, uri: &str, key: Option<&str>, body: Option<serde_json::Value>) -> Request<Body> {
	let mut builder = Request::builder().method(method).uri(uri);
	if let Some(key) = key {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
	}
	match body {
		Some(body) => builder.header(header::CONTENT_TYPE, "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap(),
		None => builder.body(Body::empty()).unwrap(),
	}
}

fn declare_body(name: &str) -> serde_json::Value {
	serde_json::json!({ "name": name, "physical_type": "f64", "timestamp_unit": "seconds" })
}

fn interpolate_body() -> serde_json::Value {
	serde_json::json!({ "points": [
		{ "timestamp": "1970-01-01T00:00:00+00:00", "value": 1.0 },
		{ "timestamp": "1970-01-01T00:01:00+00:00", "value": 2.0 },
		{ "timestamp": "1970-01-01T00:02:00+00:00", "value": 1.5 },
	] })
}

/// A router with a real store and the full keys table in force.
async fn secured_router() -> (tempfile::TempDir, axum::Router) {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let keys = KeyStore::parse(&full_keys_document()).expect("keys parse");
	let router = app_with_state(AppState::new().with_store(store).with_auth(AuthConfig::with_keys(keys)));
	(dir, router)
}

// --- The role × scope matrix (SEC-3's table, SEC-6's enforcement) -----------

/// Every scope in SEC-3's frozen table, exercised by every principal:
/// anonymous and a key the file does not hold are refused `401`
/// everywhere but the probes; each role passes exactly the scopes its
/// grant names and is refused `403` on the rest — including the two
/// discriminating cells, maintainer-without-backup and
/// writer-without-declare, that keep the grants from collapsing into
/// "any key does everything".
#[tokio::test]
async fn the_role_matrix_holds_for_every_scope() {
	let (_dir, router) = secured_router().await;
	let get = |uri: &str, key: Option<&str>| request("GET", uri, key, None);
	let post = |uri: &str, key: Option<&str>, body: serde_json::Value| request("POST", uri, key, Some(body));

	// Probe (SEC-3): the one scope anonymous holds.
	for (key, expected) in [(None, StatusCode::OK), (Some(UNKNOWN_KEY), StatusCode::UNAUTHORIZED), (Some(READER_KEY), StatusCode::OK), (Some(ADMIN_KEY), StatusCode::OK)] {
		let (status, _, _) = send(router.clone(), get("/health", key)).await;
		assert_eq!(status, expected, "GET /health as {key:?}");
	}
	// A failed authentication is refused even on a probe, with the
	// challenge the §4 table pairs with the code (SEC-6).
	let (status, body, headers) = send(router.clone(), get("/health", Some(UNKNOWN_KEY))).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["code"], "unauthenticated", "{body}");
	assert!(headers.get(header::WWW_AUTHENTICATE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("Bearer")), "the 401 carries the Bearer challenge");

	// Read: every key role passes; no key (or a bad one) does not.
	for (key, expected) in [(None, StatusCode::UNAUTHORIZED), (Some(UNKNOWN_KEY), StatusCode::UNAUTHORIZED), (Some(READER_KEY), StatusCode::OK), (Some(WRITER_KEY), StatusCode::OK), (Some(MAINTAINER_KEY), StatusCode::OK), (Some(ADMIN_KEY), StatusCode::OK)] {
		let (status, _, _) = send(router.clone(), get("/api/v1/storage/aspects", key)).await;
		assert_eq!(status, expected, "GET aspects as {key:?}");
	}

	// Compute: reader and above run stateless compute; anonymous not.
	let (status, _, _) = send(router.clone(), post("/api/v1/interpolate", None, interpolate_body())).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	for key in [READER_KEY, WRITER_KEY, ADMIN_KEY] {
		let (status, _, _) = send(router.clone(), post("/api/v1/interpolate", Some(key), interpolate_body())).await;
		assert_eq!(status, StatusCode::OK, "POST interpolate as {key}");
	}

	// Declare: writer is refused — ingest and declare are separate
	// scopes since SEC-3 froze the table — declarer and above pass.
	for (key, expected) in [(None, StatusCode::UNAUTHORIZED), (Some(READER_KEY), StatusCode::FORBIDDEN), (Some(WRITER_KEY), StatusCode::FORBIDDEN)] {
		let (status, body, _) = send(router.clone(), post("/api/v1/storage/aspects", key, declare_body("price"))).await;
		assert_eq!(status, expected, "POST declare as {key:?}");
		if expected == StatusCode::FORBIDDEN {
			assert_eq!(body["code"], "forbidden", "{body}");
		}
	}
	for (key, name) in [(DECLARER_KEY, "price"), (MAINTAINER_KEY, "temp"), (ADMIN_KEY, "hum")] {
		let (status, _, _) = send(router.clone(), post("/api/v1/storage/aspects", Some(key), declare_body(name))).await;
		assert_eq!(status, StatusCode::CREATED, "POST declare as {key}");
	}

	// Ingest: writer and above; reader is refused.
	let points = serde_json::json!({ "points": [{ "timestamp": 0, "value": "1.5" }] });
	let (status, _, _) = send(router.clone(), post("/api/v1/storage/price/points", Some(READER_KEY), points.clone())).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	let (status, _, _) = send(router.clone(), post("/api/v1/storage/price/points", None, points.clone())).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	let (status, _, _) = send(router.clone(), post("/api/v1/storage/price/points", Some(WRITER_KEY), points)).await;
	assert_eq!(status, StatusCode::CREATED, "writer ingests");

	// Metrics: maintainer and admin only (decision 17).
	for (key, expected) in [(None, StatusCode::UNAUTHORIZED), (Some(READER_KEY), StatusCode::FORBIDDEN), (Some(DECLARER_KEY), StatusCode::FORBIDDEN), (Some(MAINTAINER_KEY), StatusCode::OK), (Some(ADMIN_KEY), StatusCode::OK)] {
		let (status, _, _) = send(router.clone(), get("/metrics", key)).await;
		assert_eq!(status, expected, "GET /metrics as {key:?}");
	}

	// Maintain: maintainer and admin only.
	for (key, expected) in [(None, StatusCode::UNAUTHORIZED), (Some(READER_KEY), StatusCode::FORBIDDEN), (Some(DECLARER_KEY), StatusCode::FORBIDDEN)] {
		let (status, _, _) = send(router.clone(), post("/api/v1/storage/reconcile", key, serde_json::json!({}))).await;
		assert_eq!(status, expected, "POST reconcile as {key:?}");
	}
	for key in [MAINTAINER_KEY, ADMIN_KEY] {
		let (status, body, _) = send(router.clone(), post("/api/v1/storage/reconcile", Some(key), serde_json::json!({}))).await;
		assert_eq!(status, StatusCode::OK, "POST reconcile as {key}: {body}");
	}

	// Backup: admin alone — the maintainer's grant stops one scope
	// short, which is the cell that proves the table is enforced per
	// scope and not per "operator-ish" role.
	for (key, expected) in [(None, StatusCode::UNAUTHORIZED), (Some(READER_KEY), StatusCode::FORBIDDEN), (Some(MAINTAINER_KEY), StatusCode::FORBIDDEN)] {
		let (status, _, _) = send(router.clone(), post("/api/v1/storage/backup", key, serde_json::json!({}))).await;
		assert_eq!(status, expected, "POST backup as {key:?}");
	}
	let (status, body, _) = send(router.clone(), post("/api/v1/storage/backup", Some(ADMIN_KEY), serde_json::json!({}))).await;
	assert!(status.is_success(), "admin backs up: {status} {body}");
}

// --- Credential placement (SEC-6 / SEC-10) ----------------------------------

/// A credential travels in the `Authorization` header or nowhere: in
/// the query string it is refused `400 credentials_in_query` in every
/// auth mode — keys on, off, and explicitly disabled — because the
/// leak it prevents (query strings land in logs and proxies) does not
/// depend on the mode (SEC-10).
#[tokio::test]
async fn credentials_only_travel_in_the_authorization_header() {
	let dir = tempfile::tempdir().expect("tempdir");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let keys = KeyStore::parse(&full_keys_document()).expect("keys parse");
	for auth in [AuthConfig::with_keys(keys.clone()), AuthConfig::off(), AuthConfig::disabled()] {
		let router = app_with_state(AppState::new().with_store(store.clone()).with_auth(auth));
		let (status, body, _) = send(router, request("GET", "/api/v1/storage/aspects?api_key=whatever", None, None)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert_eq!(body["code"], "credentials_in_query", "{body}");
	}

	// A malformed Authorization header is a failed authentication, not
	// a demotion to anonymous: even the probe refuses it.
	let (_dir, router) = secured_router().await;
	let malformed = Request::builder().method("GET").uri("/health").header(header::AUTHORIZATION, "Token abc").body(Body::empty()).unwrap();
	let (status, body, _) = send(router, malformed).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["code"], "unauthenticated", "{body}");
}

// --- Key reload against live requests (SEC-7) -------------------------------

/// Reload swaps the table in force for the requests that follow it,
/// with no restart and no partially applied state: a key added by an
/// edit authenticates, a key the edit removed stops, and a malformed
/// edit is refused whole — the keys in force stay exactly what they
/// were.
#[tokio::test]
async fn key_reload_applies_to_live_requests_and_a_bad_edit_changes_nothing() {
	let dir = tempfile::tempdir().expect("tempdir");
	let keys_path = dir.path().join("keys.json");
	std::fs::write(&keys_path, keys_document(&[("reader-1", "reader", READER_KEY)])).expect("write keys");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let auth = AuthConfig::with_keys(KeyStore::load(&keys_path).expect("keys load"));
	let router = app_with_state(AppState::new().with_store(store).with_auth(auth.clone()));

	let (status, _, _) = send(router.clone(), request("GET", "/api/v1/storage/aspects", Some(READER_KEY), None)).await;
	assert_eq!(status, StatusCode::OK);
	let (status, _, _) = send(router.clone(), request("GET", "/api/v1/storage/aspects", Some(ADMIN_KEY), None)).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "the admin key does not exist yet");

	// The edit lands, then the reload: the new key works at once.
	std::fs::write(&keys_path, full_keys_document()).expect("rewrite keys");
	assert_eq!(auth.reload_keys().expect("reload succeeds"), Some(5));
	let (status, _, _) = send(router.clone(), request("POST", "/api/v1/storage/aspects", Some(ADMIN_KEY), Some(declare_body("price")))).await;
	assert_eq!(status, StatusCode::CREATED, "the added admin key declares");

	// A malformed edit changes nothing: the reload is refused and the
	// table in force is the one from before it.
	std::fs::write(&keys_path, "{ not a keys file").expect("write malformed keys");
	assert!(auth.reload_keys().is_err(), "a malformed edit is refused");
	let (status, _, _) = send(router.clone(), request("GET", "/api/v1/storage/aspects", Some(READER_KEY), None)).await;
	assert_eq!(status, StatusCode::OK, "the keys in force survive a refused reload");

	// A removal applies the same way.
	std::fs::write(&keys_path, keys_document(&[("operator", "admin", ADMIN_KEY)])).expect("rewrite keys");
	assert_eq!(auth.reload_keys().expect("reload succeeds"), Some(1));
	let (status, _, _) = send(router.clone(), request("GET", "/api/v1/storage/aspects", Some(READER_KEY), None)).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "the removed key stops authenticating");
}

// --- The audit trail of a mixed run (SEC-9) ---------------------------------

fn read_records(path: &Path) -> Vec<serde_json::Value> {
	let text = std::fs::read_to_string(path).expect("the audit file exists");
	text.lines().map(|line| serde_json::from_str(line).expect("each line is one JSON record")).collect()
}

/// One run, every record class: denials land as `auth_denied` events —
/// both the anonymous `401` and the authenticated `403` — a mutation
/// lands as an attempt/outcome pair naming its principal, a read
/// leaves no trace, and no record anywhere carries key material: not
/// a plaintext key, and not the bearer <redacted> itself.
#[tokio::test]
async fn denials_and_mutations_are_what_the_audit_log_records() {
	let dir = tempfile::tempdir().expect("tempdir");
	let audit_path = dir.path().join("audit.jsonl");
	let store = Arc::new(SegmentStore::open(dir.path().join("store")).await.expect("store opens"));
	let audit = Arc::new(AuditLog::open(&audit_path).expect("audit opens"));
	let keys = KeyStore::parse(&full_keys_document()).expect("keys parse");
	let router = app_with_state(AppState::new().with_store(store).with_audit_log(audit).with_auth(AuthConfig::with_keys(keys)));

	let (status, _, _) = send(router.clone(), request("GET", "/api/v1/storage/aspects", None, None)).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	let (status, _, _) = send(router.clone(), request("POST", "/api/v1/storage/aspects", Some(READER_KEY), Some(declare_body("price")))).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	let (status, _, _) = send(router.clone(), request("POST", "/api/v1/storage/aspects", Some(ADMIN_KEY), Some(declare_body("price")))).await;
	assert_eq!(status, StatusCode::CREATED);
	let (status, _, _) = send(router.clone(), request("GET", "/api/v1/storage/aspects", Some(READER_KEY), None)).await;
	assert_eq!(status, StatusCode::OK, "the read that follows leaves no trace");

	let records = read_records(&audit_path);
	let kinds: Vec<String> = records.iter().map(|record| format!("{}:{}", record["kind"].as_str().unwrap(), record["event"].as_str().or_else(|| record["operation"].as_str()).unwrap())).collect();
	assert_eq!(kinds, ["event:auth_denied", "event:auth_denied", "attempt:storage_aspect_declare", "outcome:storage_aspect_declare"], "{records:?}");
	assert_eq!(records[0]["code"], "unauthenticated");
	assert_eq!(records[1]["code"], "forbidden");
	assert_eq!(records[2]["principal"], "operator");
	assert_eq!(records[2]["role"], "admin");
	assert_eq!(records[3]["status"], 201);

	let text = std::fs::read_to_string(&audit_path).expect("read the audit file");
	for secret in [READER_KEY, WRITER_KEY, DECLARER_KEY, MAINTAINER_KEY, ADMIN_KEY, "Bearer"] {
		assert!(!text.contains(secret), "the audit file never carries {secret}");
	}
}

// --- Traversal names meet the gate (SEC-11, composed) ------------------------

/// SEC-11's spellings, through SEC-6's gate: anonymous is refused
/// before the name is ever judged, a role without the declare scope
/// likewise, and the admin who passes the gate meets the same
/// `400 invalid_aspect_name` — the gate changes who may ask, never
/// what names are allowed. Nothing is declared by any of it.
#[tokio::test]
async fn traversal_names_are_refused_through_the_gate() {
	let (_dir, router) = secured_router().await;
	for (key, expected, code) in [(None, StatusCode::UNAUTHORIZED, "unauthenticated"), (Some(READER_KEY), StatusCode::FORBIDDEN, "forbidden"), (Some(ADMIN_KEY), StatusCode::BAD_REQUEST, "invalid_aspect_name")] {
		let (status, body, _) = send(router.clone(), request("POST", "/api/v1/storage/aspects", key, Some(declare_body("../escape")))).await;
		assert_eq!(status, expected, "declare ../escape as {key:?}");
		assert_eq!(body["code"], code, "{body}");
	}
	let (status, body, _) = send(router.clone(), request("GET", "/api/v1/storage/aspects", Some(ADMIN_KEY), None)).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(body["aspects"].as_array().map(Vec::len), Some(0), "nothing was declared: {body}");
}

// --- The binary, composed (SEC-5 TLS + SEC-6 keys + SEC-9 audit) ------------

use std::process::{Child, Command, Stdio};

use rustls::{crypto::ring, pki_types::ServerName, ClientConfig, RootCertStore};
use tokio::{
	io::{AsyncReadExt as _, AsyncWriteExt as _}, net::TcpStream
};
use tokio_rustls::TlsConnector;

/// A running `weft-server` child and the scratch space it owns; the
/// child is killed when the guard drops.
struct ServerProcess {
	child: Child,
	log_path: PathBuf,
}

impl ServerProcess {
	fn log(&self) -> String {
		std::fs::read_to_string(&self.log_path).unwrap_or_default()
	}
}

impl Drop for ServerProcess {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

/// Spawn the real binary with a cleared environment (plus a scratch
/// HOME, so nothing it does can touch the developer's), its output
/// going to a log file the test can quote on failure.
fn spawn_server(scratch: &tempfile::TempDir, env: &[(&str, &str)]) -> ServerProcess {
	let log_path = scratch.path().join("server.log");
	let log = std::fs::File::create(&log_path).expect("create the server log");
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-server"));
	cmd.current_dir(scratch.path()).env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("HOME", scratch.path()).env("WEFT_GPU_CALIBRATE", "0").env("WEFT_RECONCILE_INTERVAL_SECS", "0").envs(env.iter().copied()).stdout(Stdio::from(log.try_clone().expect("clone the log"))).stderr(Stdio::from(log));
	// Windows cannot start Winsock without `SYSTEMROOT` (the same shim
	// as tests/tls_serve.rs).
	#[cfg(windows)]
	{
		for name in ["SYSTEMROOT", "WINDIR"] {
			if let Ok(value) = std::env::var(name) {
				cmd.env(name, value);
			}
		}
		cmd.env("USERPROFILE", scratch.path());
	}
	let child = cmd.spawn().expect("spawn weft-server");
	ServerProcess { child, log_path }
}

/// A port nobody is using, found by binding and releasing one.
fn free_port() -> u16 {
	std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe port").local_addr().expect("probe addr").port()
}

/// One raw HTTP/1.1 request over an async stream; the request asks the
/// server to close, and the response is read to the end of the stream.
async fn raw_exchange<S>(stream: &mut S, request_text: &str) -> String
where
	S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
	stream.write_all(request_text.as_bytes()).await.expect("write request");
	let mut raw = Vec::new();
	let mut chunk = [0_u8; 4096];
	loop {
		match stream.read(&mut chunk).await {
			Ok(0) | Err(_) => break,
			Ok(n) => raw.extend_from_slice(&chunk[..n]),
		}
	}
	String::from_utf8_lossy(&raw).into_owned()
}

fn http_request(method: &str, path: &str, host: &str, key: Option<&str>, extra_headers: &[(&str, &str)], body: Option<&str>) -> String {
	let mut text = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
	if let Some(key) = key {
		text.push_str(&format!("Authorization: Bearer {key}\r\n"));
	}
	for (name, value) in extra_headers {
		text.push_str(&format!("{name}: {value}\r\n"));
	}
	match body {
		Some(body) => text.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())),
		None => text.push_str("\r\n"),
	}
	text
}

/// The status and body of a raw response (chunked bodies decoded).
fn parse_response(raw: &str) -> (u16, String) {
	let status: u16 = raw.split_whitespace().nth(1).and_then(|token| token.parse().ok()).unwrap_or(0);
	let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
	if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
		let mut decoded = String::new();
		let mut rest = body;
		while let Some((size_line, after)) = rest.split_once("\r\n") {
			let Ok(size) = usize::from_str_radix(size_line.trim(), 16) else { break };
			if size == 0 {
				break;
			}
			decoded.push_str(after.get(..size).unwrap_or(after));
			rest = after.get(size..).unwrap_or_default();
			rest = rest.strip_prefix("\r\n").unwrap_or(rest);
		}
		(status, decoded)
	} else {
		(status, body.to_string())
	}
}

/// Self-signed material for `localhost`: the PEM paths to hand the
/// binary, and the client configuration that trusts the certificate.
fn tls_material(scratch: &tempfile::TempDir) -> (String, String, ClientConfig) {
	let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate a self-signed certificate");
	let cert = scratch.path().join("cert.pem");
	let key = scratch.path().join("key.pem");
	std::fs::write(&cert, certified.cert.pem()).expect("write cert");
	std::fs::write(&key, certified.signing_key.serialize_pem()).expect("write key");
	let mut roots = RootCertStore::empty();
	roots.add(certified.cert.der().clone()).expect("trust the self-signed certificate");
	let mut client = ClientConfig::builder_with_provider(Arc::new(ring::default_provider())).with_safe_default_protocol_versions().expect("protocol versions").with_root_certificates(roots).with_no_client_auth();
	client.alpn_protocols = vec![b"http/1.1".to_vec()];
	(cert.to_str().expect("utf8 path").to_string(), key.to_str().expect("utf8 path").to_string(), client)
}

/// One HTTPS request to the binary; `Err` when the connection or the
/// handshake fails (a not-yet-ready server, or a refused one).
async fn https_request(addr: std::net::SocketAddr, client: &ClientConfig, request_text: &str) -> Result<(u16, String), String> {
	let connector = TlsConnector::from(Arc::new(client.clone()));
	let tcp = TcpStream::connect(addr).await.map_err(|error| error.to_string())?;
	let mut stream = connector.connect(ServerName::try_from("localhost").expect("server name"), tcp).await.map_err(|error| error.to_string())?;
	Ok(parse_response(&raw_exchange(&mut stream, request_text).await))
}

/// One plaintext HTTP request to the binary; `Err` when the
/// connection fails (a not-yet-ready server).
async fn try_http_request_over_plaintext(addr: std::net::SocketAddr, request_text: &str) -> Result<(u16, String), String> {
	let mut stream = TcpStream::connect(addr).await.map_err(|error| error.to_string())?;
	Ok(parse_response(&raw_exchange(&mut stream, request_text).await))
}

/// One plaintext HTTP request to the binary.
async fn http_request_over_plaintext(addr: std::net::SocketAddr, request_text: &str) -> (u16, String) {
	try_http_request_over_plaintext(addr, request_text).await.expect("connect")
}

/// Poll a probe until it answers `200` or the deadline passes.
async fn wait_until_serving<F, Fut>(server: &ServerProcess, mut probe: F)
where
	F: FnMut() -> Fut,
	Fut: std::future::Future<Output = Result<(u16, String), String>>,
{
	let deadline = std::time::Instant::now() + Duration::from_secs(60);
	loop {
		if let Ok((status, _)) = probe().await {
			if status == 200 {
				return;
			}
		}
		assert!(std::time::Instant::now() < deadline, "the server never came up:\n{}", server.log());
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

/// The deployment SEC-13 accepts, booted whole: TLS on the serve path
/// (SEC-5), a mounted keys file enforced per scope (SEC-3/SEC-6), a
/// SIGHUP key reload with no restart (SEC-7), and the fsynced audit
/// file recording the run's denials and mutations (SEC-9) — with no
/// plaintext served on the TLS port and no key material in the log.
#[tokio::test]
async fn the_binary_serves_the_whole_surface_over_tls() {
	let scratch = tempfile::tempdir().expect("scratch dir");
	let store_root = scratch.path().join("store");
	let keys_path = scratch.path().join("keys.json");
	std::fs::write(&keys_path, full_keys_document()).expect("write keys");
	let (cert, key_pem, client) = tls_material(&scratch);
	let port = free_port();
	let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
	let store_root_text = store_root.to_str().expect("utf8 path").to_string();
	let keys_text = keys_path.to_str().expect("utf8 path").to_string();
	let server = spawn_server(&scratch, &[("WEFT_SERVER_ADDR", &addr.to_string()), ("WEFT_SEGMENT_STORE_ROOT", &store_root_text), ("WEFT_KEYS_FILE", &keys_text), ("WEFT_TLS_CERT_FILE", &cert), ("WEFT_TLS_KEY_FILE", &key_pem)]);

	wait_until_serving(&server, || {
		let text = http_request("GET", "/health", "localhost", None, &[], None);
		let client = &client;
		async move { https_request(addr, client, &text).await }
	})
	.await;

	let call = |method: &str, path: &str, key: Option<&str>, body: Option<&str>| {
		let text = http_request(method, path, "localhost", key, &[], body);
		let client = &client;
		async move { https_request(addr, client, &text).await.expect("the TLS request completes") }
	};

	// The matrix's decisive cells, over the wire this time.
	let (status, _) = call("GET", "/api/v1/storage/aspects", None, None).await;
	assert_eq!(status, 401, "anonymous reads nothing");
	let (status, _) = call("GET", "/api/v1/storage/aspects", Some(READER_KEY), None).await;
	assert_eq!(status, 200, "the reader reads");
	let declare = serde_json::json!({ "name": "price", "physical_type": "f64", "timestamp_unit": "seconds" }).to_string();
	let (status, _) = call("POST", "/api/v1/storage/aspects", Some(READER_KEY), Some(&declare)).await;
	assert_eq!(status, 403, "the reader declares nothing");
	let (status, _) = call("POST", "/api/v1/storage/aspects", Some(ADMIN_KEY), Some(&declare)).await;
	assert_eq!(status, 201, "the admin declares");
	let (status, _) = call("POST", "/api/v1/storage/price/points", Some(WRITER_KEY), Some(r#"{"points":[{"timestamp":0,"value":"1.5"}]}"#)).await;
	assert_eq!(status, 201, "the writer ingests");
	let (status, body) = call("GET", "/api/v1/storage/price/points?start=0&end=100", Some(READER_KEY), None).await;
	assert_eq!(status, 200, "the reader reads the points back: {body}");
	let (status, _) = call("GET", "/metrics", Some(READER_KEY), None).await;
	assert_eq!(status, 403, "metrics is not the reader's scope");
	let (status, _) = call("GET", "/metrics", Some(MAINTAINER_KEY), None).await;
	assert_eq!(status, 200, "metrics is the maintainer's scope");

	// Plaintext to the TLS port is not served (SEC-5): a TLS alert at
	// most, never an HTTP response.
	let (status, _) = http_request_over_plaintext(addr, &http_request("GET", "/health", "localhost", None, &[], None)).await;
	assert_eq!(status, 0, "no HTTP status answers plaintext on the TLS port");

	// SEC-7 over the wire: edit the mounted file, SIGHUP, and the new
	// key authenticates against the same process — no restart.
	#[cfg(unix)]
	{
		let rotated = "sec13-rotated-admin-key";
		std::fs::write(&keys_path, keys_document(&[("reader-1", "reader", READER_KEY), ("operator", "admin", rotated)])).expect("rewrite keys");
		assert_eq!(unsafe { libc::kill(server.child.id() as i32, libc::SIGHUP) }, 0, "SIGHUP is delivered");
		let deadline = std::time::Instant::now() + Duration::from_secs(15);
		loop {
			let (status, _) = call("GET", "/api/v1/storage/aspects", Some(rotated), None).await;
			if status == 200 {
				break;
			}
			assert!(std::time::Instant::now() < deadline, "the rotated key never took effect:\n{}", server.log());
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
		let (status, _) = call("GET", "/api/v1/storage/aspects", Some(ADMIN_KEY), None).await;
		assert_eq!(status, 401, "the replaced key stops authenticating");
	}

	// SEC-9 on disk: the run's audit file, in the store root, holds
	// the denial events and the declare's attempt/outcome pair — and
	// none of the keys it enforced.
	let audit_text = std::fs::read_to_string(store_root.join("audit.jsonl")).expect("the audit file is in the store root");
	let records: Vec<serde_json::Value> = audit_text.lines().map(|line| serde_json::from_str(line).expect("each line is one JSON record")).collect();
	assert!(records.iter().any(|record| record["event"] == "auth_denied" && record["code"] == "unauthenticated"), "{records:?}");
	assert!(records.iter().any(|record| record["event"] == "auth_denied" && record["code"] == "forbidden"), "{records:?}");
	assert!(records.iter().any(|record| record["kind"] == "attempt" && record["operation"] == "storage_aspect_declare" && record["principal"] == "operator"), "{records:?}");
	for secret in [READER_KEY, WRITER_KEY, ADMIN_KEY, "sec13-rotated-admin-key"] {
		assert!(!audit_text.contains(secret), "the audit file never carries a key");
	}
}

/// The auth-off half of SEC-8's matrix, on the binary's default
/// loopback bind: a request a proxy forwarded is refused
/// `403 proxied_request_requires_auth` and a foreign `Host` is refused
/// `403 host_not_allowed` — the loopback assumption both guards exist
/// to protect — while the explicitly disabled gateway mode
/// (`WEFT_AUTH=disabled`) serves the proxied request, because there
/// the proxy owning authentication is the deployment.
#[tokio::test]
async fn the_auth_off_loopback_guards_hold_on_the_binary() {
	let scratch = tempfile::tempdir().expect("scratch dir");
	let port = free_port();
	let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
	let probe = |addr: std::net::SocketAddr| {
		let text = http_request("GET", "/health", "localhost", None, &[], None);
		async move { try_http_request_over_plaintext(addr, &text).await }
	};
	let server = spawn_server(&scratch, &[("WEFT_SERVER_ADDR", &addr.to_string())]);
	wait_until_serving(&server, || probe(addr)).await;

	let (status, _) = http_request_over_plaintext(addr, &http_request("GET", "/health", "localhost", None, &[], None)).await;
	assert_eq!(status, 200, "a plain loopback probe serves");
	let (status, body) = http_request_over_plaintext(addr, &http_request("GET", "/health", "localhost", None, &[("X-Forwarded-For", "203.0.113.9")], None)).await;
	assert_eq!(status, 403, "a proxied request is refused while auth is off: {body}");
	assert!(body.contains("proxied_request_requires_auth"), "{body}");
	let (status, body) = http_request_over_plaintext(addr, &http_request("GET", "/health", "other.example", None, &[], None)).await;
	assert_eq!(status, 403, "a foreign Host is refused on a loopback bind: {body}");
	assert!(body.contains("host_not_allowed"), "{body}");
	drop(server);

	// The gateway mode is the one deployment the refusal does not
	// cover: auth explicitly disabled, the proxy is the point.
	let port = free_port();
	let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
	let server = spawn_server(&scratch, &[("WEFT_SERVER_ADDR", &addr.to_string()), ("WEFT_AUTH", "disabled")]);
	wait_until_serving(&server, || probe(addr)).await;
	let (status, _) = http_request_over_plaintext(addr, &http_request("GET", "/health", "localhost", None, &[("X-Forwarded-For", "203.0.113.9")], None)).await;
	assert_eq!(status, 200, "WEFT_AUTH=disabled serves the proxied request it exists for");
}
