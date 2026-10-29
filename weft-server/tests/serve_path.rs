//! SEC-4: `weft_server::serve::run` is the only serve path, and its
//! pre-auth listener limits are observable from the wire — the connection
//! cap refuses to *serve* (not merely to answer) beyond the limit, a peer
//! that never finishes its headers is cut at the header deadline, an idle
//! keep-alive connection is cut at the keep-alive deadline, and a shutdown
//! signal recorded before serving starts makes serving exit cleanly
//! without accepting.
//!
//! These tests drive the real serve path over real TCP with raw requests,
//! so what they pin is the wire behaviour, not the internals.

use std::{num::NonZeroUsize, time::Duration};

use tokio::{
	io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, sync::oneshot
};
use weft_server::{
	app, serve::{self, ServeLimits}, ShutdownSignals
};

/// Spawn `serve::run` on an ephemeral port with `limits`; the returned
/// shutdown sender stops it and the handle reports its result.
async fn spawn_server(limits: ServeLimits) -> (std::net::SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<std::io::Result<()>>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("local addr");
	let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
	let handle = tokio::spawn(serve::run(listener, app(), limits, async move {
		let _ = shutdown_rx.await;
	}));
	(addr, shutdown_tx, handle)
}

fn limits(max_connections: usize) -> ServeLimits {
	ServeLimits::new(NonZeroUsize::new(max_connections).expect("positive cap"))
}

/// Read one HTTP/1.1 response (status line, headers, content-length body).
async fn read_response(stream: &mut TcpStream) -> String {
	let mut buf = Vec::new();
	let mut chunk = [0_u8; 1024];
	loop {
		if let Some(head_end) = find(&buf, b"\r\n\r\n") {
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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|window| window == needle)
}

const REQUEST: &str = "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";

#[tokio::test]
async fn it_serves_and_keeps_the_connection_alive() {
	let (addr, shutdown, handle) = spawn_server(limits(8)).await;
	let mut stream = TcpStream::connect(addr).await.expect("connect");
	for _ in 0..2 {
		stream.write_all(REQUEST.as_bytes()).await.expect("write request");
		let response = read_response(&mut stream).await;
		assert!(response.starts_with("HTTP/1.1 200"), "{response}");
		assert!(response.contains("\"status\":\"ok\""), "{response}");
	}
	drop(stream);
	let _ = shutdown.send(());
	handle.await.expect("server task").expect("clean serve exit");
}

#[tokio::test]
async fn the_connection_cap_leaves_excess_connections_unserved_until_a_slot_frees() {
	let (addr, shutdown, handle) = spawn_server(limits(1)).await;
	// The one permitted connection: served, then held open on keep-alive.
	let mut first = TcpStream::connect(addr).await.expect("connect first");
	first.write_all(REQUEST.as_bytes()).await.expect("write first");
	assert!(read_response(&mut first).await.starts_with("HTTP/1.1 200"));

	// A second connection can complete its TCP handshake (the kernel
	// backlog holds it) and send a request, but pre-auth it is not served:
	// no response bytes arrive while the cap is exhausted.
	let mut second = TcpStream::connect(addr).await.expect("connect second");
	second.write_all(REQUEST.as_bytes()).await.expect("write second");
	let mut byte = [0_u8; 1];
	let unserved = tokio::time::timeout(Duration::from_millis(300), second.read(&mut byte)).await;
	assert!(unserved.is_err(), "the excess connection was served while the cap was exhausted");

	// Freeing the slot serves it.
	drop(first);
	let response = tokio::time::timeout(Duration::from_secs(5), read_response(&mut second)).await.expect("the freed slot serves the waiting connection");
	assert!(response.starts_with("HTTP/1.1 200"), "{response}");

	drop(second);
	let _ = shutdown.send(());
	handle.await.expect("server task").expect("clean serve exit");
}

#[tokio::test]
async fn a_peer_that_never_finishes_its_headers_is_cut_at_the_header_deadline() {
	let limited = limits(8).with_header_read_timeout(Duration::from_millis(150));
	let (addr, shutdown, handle) = spawn_server(limited).await;
	let mut stream = TcpStream::connect(addr).await.expect("connect");
	// A complete header block never arrives: no terminating blank line.
	stream.write_all(b"GET /health HTTP/1.1\r\nHost: localh").await.expect("write partial headers");
	let mut byte = [0_u8; 1];
	let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await.expect("the header deadline closes the connection promptly");
	assert_eq!(read.expect("read does not error, it ends"), 0, "the server closed the stalled-header connection");
	let _ = shutdown.send(());
	handle.await.expect("server task").expect("clean serve exit");
}

#[tokio::test]
async fn an_idle_keep_alive_connection_is_cut_at_the_keep_alive_deadline() {
	let limited = limits(8).with_keep_alive_timeout(Duration::from_millis(150));
	let (addr, shutdown, handle) = spawn_server(limited).await;
	let mut stream = TcpStream::connect(addr).await.expect("connect");
	stream.write_all(REQUEST.as_bytes()).await.expect("write request");
	assert!(read_response(&mut stream).await.starts_with("HTTP/1.1 200"));
	// Now sit idle on the keep-alive connection: the deadline closes it.
	let mut byte = [0_u8; 1];
	let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await.expect("the keep-alive deadline closes the connection promptly");
	assert_eq!(read.expect("read does not error, it ends"), 0, "the server closed the idle connection");
	let _ = shutdown.send(());
	handle.await.expect("server task").expect("clean serve exit");
}

/// C9: handlers installed first, and a signal before serving exits 0.
/// The handlers are installed, the signal is delivered, and only then is
/// serving entered: `run` must return `Ok` at once, without the process
/// dying (the installed handler replaced the default terminate) and
/// without serving the request waiting in the backlog.
#[cfg(unix)]
#[tokio::test]
async fn a_signal_before_serving_exits_zero() {
	let mut signals = ShutdownSignals::install().expect("install handlers");
	// SAFETY: raise only delivers SIGTERM to this process; the handler
	// installed above is what receives it.
	assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0, "the signal is delivered");
	// Give the runtime's signal driver a moment to record the delivery
	// before serving is entered — the ordering the startup phases create
	// by installing handlers before opening the store.
	tokio::time::sleep(Duration::from_millis(100)).await;

	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("local addr");
	let mut waiting = TcpStream::connect(addr).await.expect("connect");
	waiting.write_all(REQUEST.as_bytes()).await.expect("write request");

	let served = tokio::time::timeout(Duration::from_secs(5), serve::run(listener, app(), limits(8), signals.wait())).await;
	served.expect("serving exits promptly on the recorded signal").expect("a signal before serving exits 0 (Ok)");

	// And nothing was served on the way out: the waiting connection gets no
	// response byte — it is left hanging, closed, or reset with its request
	// unread in the dropped listener's backlog, but never answered.
	let mut byte = [0_u8; 1];
	let read = tokio::time::timeout(Duration::from_secs(2), waiting.read(&mut byte)).await;
	let served_bytes = matches!(read, Ok(Ok(n)) if n > 0);
	assert!(!served_bytes, "no request was served after the pre-serving signal");
}
