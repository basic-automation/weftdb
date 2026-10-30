//! SEC-5: the TLS listener is the SEC-4 serve path with a rustls (ring)
//! handshake in front of it, and the binary's plaintext gate refuses an
//! unopted non-loopback exposure at startup.
//!
//! The serve-path tests drive `serve::run_tls` over real TCP with a real
//! rustls client, so what they pin is the wire behaviour: HTTPS answers,
//! plaintext to the same port does not, and a peer that connects and says
//! nothing is cut by the same keep-alive deadline as a plaintext peer.
//! The startup tests run the binary itself: 0.0.0.0 with no opt-in exits
//! with the refusal; each opt-in starts serving.

use std::{
	io::Read as _, num::NonZeroUsize, process::{Command, Stdio}, sync::Arc, time::Duration
};

use rustls::{crypto::ring, pki_types::ServerName, ClientConfig, RootCertStore};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, sync::oneshot
};
use tokio_rustls::TlsConnector;
use weft_server::{
	app, serve::{self, ServeLimits}, TlsConfig
};

const REQUEST: &str = "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";

/// A self-signed server configuration for `localhost`, plus the client
/// configuration that trusts exactly that certificate.
fn tls_pair() -> (TlsConfig, ClientConfig) {
	let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate a self-signed certificate");
	let server = TlsConfig::from_pem(certified.cert.pem().as_bytes(), certified.signing_key.serialize_pem().as_bytes()).expect("server config");
	let mut roots = RootCertStore::empty();
	roots.add(certified.cert.der().clone()).expect("trust the self-signed certificate");
	let mut client = ClientConfig::builder_with_provider(Arc::new(ring::default_provider())).with_safe_default_protocol_versions().expect("protocol versions").with_root_certificates(roots).with_no_client_auth();
	client.alpn_protocols = vec![b"http/1.1".to_vec()];
	(server, client)
}

/// Spawn `serve::run_tls` on an ephemeral port; the returned shutdown
/// sender stops it and the handle reports its result.
async fn spawn_tls_server(limits: ServeLimits, tls: &TlsConfig) -> (std::net::SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<std::io::Result<()>>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("local addr");
	let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
	let tls = tls.clone();
	let handle = tokio::spawn(async move {
		serve::run_tls(listener, app(), limits, &tls, async move {
			let _ = shutdown_rx.await;
		})
		.await
	});
	(addr, shutdown_tx, handle)
}

fn limits(max_connections: usize) -> ServeLimits {
	ServeLimits::new(NonZeroUsize::new(max_connections).expect("positive cap"))
}

/// Read one HTTP/1.1 response (status line, headers, content-length body)
/// off any async stream.
async fn read_response<S>(stream: &mut S) -> String
where
	S: AsyncReadExt + Unpin,
{
	let mut buf = Vec::new();
	let mut chunk = [0_u8; 1024];
	loop {
		if let Some(head_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
			let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
			let length: usize = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().expect("content-length"))).expect("a content-length header");
			if buf.len() >= head_end + 4 + length {
				return String::from_utf8_lossy(&buf[..head_end + 4 + length]).into_owned();
			}
		}
		let read = stream.read(&mut chunk).await.expect("read response");
		assert!(read > 0, "the connection closed mid-response: {}", String::from_utf8_lossy(&buf));
		buf.extend_from_slice(&chunk[..read]);
	}
}

#[tokio::test]
async fn https_is_served_on_the_shared_path() {
	let (server, client) = tls_pair();
	let (addr, shutdown, handle) = spawn_tls_server(limits(8), &server).await;
	let connector = TlsConnector::from(Arc::new(client));
	let tcp = TcpStream::connect(addr).await.expect("connect");
	let mut tls_stream = connector.connect(ServerName::try_from("localhost").expect("server name"), tcp).await.expect("the handshake completes");
	// The negotiated protocol is HTTP/1.1: decision 17, ALPN-pinned.
	assert_eq!(tls_stream.get_ref().1.alpn_protocol(), Some(b"http/1.1".as_slice()));
	for _ in 0..2 {
		tls_stream.write_all(REQUEST.as_bytes()).await.expect("write request");
		let response = read_response(&mut tls_stream).await;
		assert!(response.starts_with("HTTP/1.1 200"), "{response}");
		assert!(response.contains("\"status\":\"ok\""), "{response}");
	}
	drop(tls_stream);
	let _ = shutdown.send(());
	handle.await.expect("server task").expect("clean serve exit");
}

#[tokio::test]
async fn plaintext_to_the_tls_port_is_not_served() {
	let (server, _client) = tls_pair();
	let (addr, shutdown, handle) = spawn_tls_server(limits(8), &server).await;
	let mut stream = TcpStream::connect(addr).await.expect("connect");
	stream.write_all(REQUEST.as_bytes()).await.expect("write request");
	// The handshake cannot complete over plaintext HTTP bytes: the server
	// answers with a TLS alert (a few bytes, not an HTTP response) and
	// closes the connection.
	let mut received = Vec::new();
	let mut chunk = [0_u8; 64];
	let read_all = tokio::time::timeout(Duration::from_secs(5), async {
		loop {
			match stream.read(&mut chunk).await {
				Ok(0) | Err(_) => break,
				Ok(n) => received.extend_from_slice(&chunk[..n]),
			}
		}
	})
	.await;
	assert!(read_all.is_ok(), "the failed handshake closes the connection promptly");
	assert!(!received.windows(8).any(|w| w == b"HTTP/1.1"), "no plaintext response is served: {received:?}");
	let _ = shutdown.send(());
	handle.await.expect("server task").expect("clean serve exit");
}

#[tokio::test]
async fn a_peer_that_never_speaks_is_cut_by_the_keep_alive_deadline() {
	let (server, _client) = tls_pair();
	let limited = limits(8).with_keep_alive_timeout(Duration::from_millis(150));
	let (addr, shutdown, handle) = spawn_tls_server(limited, &server).await;
	// Connect and say nothing: the handshake cannot start, and the
	// deadline that bounds an idle plaintext peer bounds this one too.
	let mut stream = TcpStream::connect(addr).await.expect("connect");
	let mut byte = [0_u8; 1];
	let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await.expect("the deadline closes the connection promptly");
	assert!(matches!(read, Ok(0) | Err(_)), "the stalled-handshake connection is closed: {read:?}");
	let _ = shutdown.send(());
	handle.await.expect("server task").expect("clean serve exit");
}

// --- The binary's plaintext gate -------------------------------------------

/// Run the real binary with `env` (plus a scratch HOME so nothing it does
/// can touch the developer's) and report how it ended, or that it is still
/// serving after `wait`.
fn run_binary(env: &[(&str, &str)], wait: Duration) -> (bool, String) {
	let scratch = tempfile::tempdir().expect("scratch dir");
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-server"));
	cmd.env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("HOME", scratch.path()).envs(env.iter().copied()).stdout(Stdio::piped()).stderr(Stdio::piped());
	let mut child = cmd.spawn().expect("spawn weft-server");
	let deadline = std::time::Instant::now() + wait;
	while std::time::Instant::now() < deadline {
		if child.try_wait().expect("poll child").is_some() {
			let mut out = String::new();
			if let Some(mut stdout) = child.stdout.take() {
				let _ = stdout.read_to_string(&mut out);
			}
			if let Some(mut stderr) = child.stderr.take() {
				let _ = stderr.read_to_string(&mut out);
			}
			return (true, out);
		}
		std::thread::sleep(Duration::from_millis(50));
	}
	child.kill().expect("kill the still-serving child");
	let _ = child.wait();
	(false, String::new())
}

#[test]
fn a_non_loopback_plaintext_bind_without_an_opt_in_is_refused() {
	let (exited, output) = run_binary(&[("WEFT_SERVER_ADDR", "0.0.0.0:0")], Duration::from_secs(30));
	assert!(exited, "the server must refuse and exit, not serve");
	assert!(output.contains("refusing to serve plaintext"), "the refusal says why:\n{output}");
}

#[test]
fn auth_disabled_lets_a_non_loopback_plaintext_bind_start() {
	let (exited, _) = run_binary(&[("WEFT_SERVER_ADDR", "0.0.0.0:0"), ("WEFT_AUTH", "disabled")], Duration::from_secs(5));
	assert!(!exited, "with WEFT_AUTH=disabled the server starts and keeps serving");
}

#[test]
fn a_mounted_keys_file_with_the_plaintext_opt_in_starts() {
	let scratch = tempfile::tempdir().expect("scratch dir");
	let keys = scratch.path().join("keys.json");
	// A real SEC-6 keys file: since SEC-6 the server loads and validates
	// the file at startup, so the placeholder `{}` SEC-5's gate accepted
	// (it read presence only) no longer starts a server.
	let document = format!("{{\"version\": 1, \"keys\": [{{\"id\": \"operator\", \"role\": \"admin\", \"key_hash\": \"{}\"}}]}}", weft_server::auth::format_key_hash("weft_keys_file_fixture_key"));
	std::fs::write(&keys, document).expect("write the keys file");
	let keys = keys.to_str().expect("utf8 path").to_string();
	let (exited, _) = run_binary(&[("WEFT_SERVER_ADDR", "0.0.0.0:0"), ("WEFT_KEYS_FILE", &keys), ("WEFT_ALLOW_PLAINTEXT", "1")], Duration::from_secs(5));
	assert!(!exited, "keys file + WEFT_ALLOW_PLAINTEXT=1 starts and keeps serving");
}

#[test]
fn a_tls_bind_starts_without_any_plaintext_opt_in() {
	let scratch = tempfile::tempdir().expect("scratch dir");
	let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate");
	let cert = scratch.path().join("cert.pem");
	let key = scratch.path().join("key.pem");
	std::fs::write(&cert, certified.cert.pem()).expect("write cert");
	std::fs::write(&key, certified.signing_key.serialize_pem()).expect("write key");
	let cert = cert.to_str().expect("utf8 path").to_string();
	let key = key.to_str().expect("utf8 path").to_string();
	let (exited, _) = run_binary(&[("WEFT_SERVER_ADDR", "0.0.0.0:0"), ("WEFT_TLS_CERT_FILE", &cert), ("WEFT_TLS_KEY_FILE", &key)], Duration::from_secs(5));
	assert!(!exited, "a TLS bind starts and keeps serving");
}
