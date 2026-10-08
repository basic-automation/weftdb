//! Loopback request guard: keep web pages and DNS rebinding away from a loopback-bound
//! server.
//!
//! `weft-server` has no authentication yet and binds `127.0.0.1:8080` by default, so
//! that only local processes can reach it. A web browser on the same machine is a local
//! process too. A page it shows can send a cross-origin request to the server without a
//! CORS preflight (a `POST` whose parameters are all in the query string, or whose body
//! is `text/plain`), which is enough to drive maintenance, backup and ingest; and DNS
//! rebinding can make the server look same-origin to a page on another domain.
//!
//! While the server is bound to a loopback address, [`enforce`] refuses:
//!
//! - **a request not addressed to a loopback host**, with `421 Misdirected Request`. The
//!   `Host` header must be `localhost`, an IPv4 address in `127.0.0.0/8` or `[::1]` (or
//!   the IPv4-mapped form of a `127.0.0.0/8` address, `[::ffff:127.0.0.1]`), each with an
//!   optional port (the name is compared case-insensitively). A request target
//!   that carries its own authority (absolute form, HTTP/2) is held to the same rule. A
//!   request with no host at all, or with more than one `Host` header, is refused too;
//!   HTTP/1.1 requires exactly one, and browsers always send it.
//! - **a state-changing request from a non-loopback web origin**, with `403 Forbidden`:
//!   any method but `GET`, `HEAD`, `OPTIONS` and `TRACE` that carries an `Origin` header
//!   whose host is not a loopback host, including the opaque `null` origin. Browsers
//!   send `Origin` with every cross-origin `POST`; `curl` and other non-browser clients
//!   send none and are not affected. Any loopback origin passes, on any port, so a page
//!   served by another local web server is still trusted: comparing the port with the
//!   bound one would break reverse proxies, and authentication is what will close it.
//!
//! `/health` and `/ready` get no exemption. A probe on the same host reaches a
//! loopback-bound server as `localhost` or `127.0.0.1` and passes if it sends that
//! `Host`, as every HTTP/1.1 client does; an HTTP/1.0 probe that sends no `Host` gets
//! `421` and must be configured to send one (or the guard turned off). A probe from
//! another network namespace (a container health check from outside, a kubelet) can only
//! reach a server bound to a non-loopback address, where the guard is off.
//!
//! The guard is off for a non-loopback bind, which is unchanged (there is no
//! authentication yet, so such a server must not be reachable from an untrusted network;
//! see `SECURITY.md`), and when `WEFT_ALLOW_ANY_HOST=1`, for a local reverse proxy that
//! forwards a different `Host`, or that forwards the `Origin` of a browser UI served
//! from a non-loopback origin (its state-changing requests would otherwise get `403`).
//! A bind to the IPv4-mapped form of a loopback address (`[::ffff:127.0.0.1]`) counts
//! as loopback. It is a stop-gap until authentication exists, not a
//! replacement for it. [`AppState`](crate::AppState) starts with [`HostGuard::Off`], so
//! a router built with [`app_with_state`](crate::app_with_state) is unguarded unless the
//! caller sets [`AppState::with_host_guard`](crate::AppState::with_host_guard); the
//! binary sets it from the address it bound.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::{
	extract::{Request, State}, http::{header, HeaderValue, StatusCode}, middleware::Next, response::{IntoResponse, Response}, Json
};

/// Environment variable that turns the guard off on a loopback bind: `1`, `true`, `yes`
/// or `on` (any case). For a local reverse proxy that forwards a non-loopback `Host`, or
/// a non-loopback browser `Origin`.
pub const ALLOW_ANY_HOST_ENV: &str = "WEFT_ALLOW_ANY_HOST";

/// The longest prefix of a refused `Host`/`Origin` value an error message quotes.
const SHOWN_VALUE_CHARS: usize = 64;

/// A refused request: the status to answer with and the message for the error envelope.
type Refusal = (StatusCode, String);

/// Whether the server checks `Host` and `Origin` (see the [module docs](self)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostGuard {
	/// No checks: a non-loopback bind, `WEFT_ALLOW_ANY_HOST` set, or a router built
	/// without a guard.
	#[default]
	Off,
	/// Only loopback hosts are served, and state-changing requests from other web
	/// origins are refused.
	Loopback,
}

impl HostGuard {
	/// The guard for a server bound to `addr`: [`Loopback`](Self::Loopback) for a loopback
	/// address (including the IPv4-mapped `::ffff:127.0.0.0/104`) unless
	/// `allow_any_host`, otherwise [`Off`](Self::Off).
	#[must_use]
	pub const fn for_bind(addr: SocketAddr, allow_any_host: bool) -> Self {
		if addr.ip().to_canonical().is_loopback() && !allow_any_host {
			Self::Loopback
		} else {
			Self::Off
		}
	}
}

/// Read a `WEFT_ALLOW_ANY_HOST` value: `1`, `true`, `yes` or `on` (any case, surrounding
/// whitespace ignored) allow any host; unset or anything else does not.
#[must_use]
pub fn allow_any_host_from_env(value: Option<&str>) -> bool {
	value.is_some_and(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// axum middleware: apply `guard` to the request, answering `421`/`403` in the usual
/// JSON error envelope when it is refused and passing it on otherwise.
pub async fn enforce(State(guard): State<HostGuard>, request: Request, next: Next) -> Response {
	if guard == HostGuard::Loopback {
		if let Err((status, error)) = check_loopback(&request) {
			// The `{"error": …}` envelope the other endpoints use.
			return (status, Json(serde_json::json!({ "error": error }))).into_response();
		}
	}
	next.run(request).await
}

/// The [`HostGuard::Loopback`] checks, in order: exactly one host, every named host a
/// loopback host, and no foreign `Origin` on a state-changing method.
fn check_loopback(request: &Request) -> Result<(), Refusal> {
	let mut hosts = request.headers().get_all(header::HOST).iter();
	let host = hosts.next();
	if hosts.next().is_some() {
		return Err(misdirected("the request carries more than one Host header"));
	}
	let authority = request.uri().authority();
	if host.is_none() && authority.is_none() {
		return Err(misdirected("the request names no host"));
	}
	if let Some(host) = host {
		if !host.to_str().is_ok_and(is_loopback_authority) {
			return Err(misdirected(&format!("Host {} is not a loopback host", shown(host))));
		}
	}
	if let Some(authority) = authority {
		if !is_loopback_authority(authority.as_str()) {
			return Err(misdirected(&format!("the request target's host {:?} is not a loopback host", authority.as_str())));
		}
	}
	if !request.method().is_safe() {
		for origin in request.headers().get_all(header::ORIGIN) {
			if !origin.to_str().is_ok_and(is_loopback_origin) {
				return Err((StatusCode::FORBIDDEN, format!("this server is bound to a loopback address and refuses a {} request from the web origin {} (behind a local reverse proxy that forwards a browser's non-loopback Origin, set {ALLOW_ANY_HOST_ENV}=1)", request.method(), shown(origin))));
			}
		}
	}
	Ok(())
}

/// Whether `authority` (a `Host` value, or the `host[:port]` of an origin or request
/// target) names a loopback host: `localhost` (any case), an IPv4 address in
/// `127.0.0.0/8`, `[::1]`, or the IPv4-mapped form of a `127.0.0.0/8` address, with an
/// optional port.
fn is_loopback_authority(authority: &str) -> bool {
	if let Some(bracketed) = authority.strip_prefix('[') {
		let Some((address, rest)) = bracketed.split_once(']') else { return false };
		return is_port_suffix(rest) && address.parse::<Ipv6Addr>().is_ok_and(|ip| ip.to_canonical().is_loopback());
	}
	let (host, rest) = authority.find(':').map_or((authority, ""), |colon| authority.split_at(colon));
	is_port_suffix(rest) && (host.eq_ignore_ascii_case("localhost") || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback()))
}

/// Whether `rest` is empty or `:` followed by at most five ASCII digits (RFC 3986 allows
/// an empty port).
fn is_port_suffix(rest: &str) -> bool {
	rest.is_empty() || rest.strip_prefix(':').is_some_and(|port| port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether an `Origin` header value is an `http`/`https` origin on a loopback host. The
/// opaque `null` origin and every other scheme are not.
fn is_loopback_origin(origin: &str) -> bool {
	origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")).is_some_and(is_loopback_authority)
}

/// A header value for an error message: lossily decoded, quoted, and cut to
/// [`SHOWN_VALUE_CHARS`] characters.
fn shown(value: &HeaderValue) -> String {
	let text = String::from_utf8_lossy(value.as_bytes());
	let prefix: String = text.chars().take(SHOWN_VALUE_CHARS).collect();
	let ellipsis = if text.chars().nth(SHOWN_VALUE_CHARS).is_some() { "…" } else { "" };
	format!("{prefix:?}{ellipsis}")
}

/// A `421 Misdirected Request` naming what was wrong with the host and the escape hatch.
fn misdirected(problem: &str) -> Refusal {
	(StatusCode::MISDIRECTED_REQUEST, format!("{problem}: this server is bound to a loopback address and only answers requests addressed to localhost, 127.0.0.0/8 or [::1] (behind a local reverse proxy that rewrites Host, set {ALLOW_ANY_HOST_ENV}=1)"))
}

#[cfg(test)]
mod tests {
	use axum::{
		body::Body, http::{Method, Request as HttpRequest}
	};
	use tower::ServiceExt as _;

	use super::*;
	use crate::{app_with_state, AppState, SharedMetrics};

	/// A router guarded as the binary guards a loopback bind, counting into `metrics`.
	fn guarded(metrics: &SharedMetrics) -> axum::Router {
		app_with_state(AppState::with_metrics(metrics.clone()).with_host_guard(HostGuard::Loopback))
	}

	/// `GET uri` with the given `Host` header (none when `None`).
	fn get(uri: &str, host: Option<&str>) -> HttpRequest<Body> {
		let builder = HttpRequest::builder().uri(uri);
		let builder = match host {
			Some(host) => builder.header(header::HOST, host),
			None => builder,
		};
		builder.body(Body::empty()).unwrap()
	}

	/// A valid `POST /api/v1/interpolate` addressed to `host`, from `origin` when given.
	fn interpolate(host: &str, origin: Option<&str>) -> HttpRequest<Body> {
		let body = serde_json::json!({
			"spline": "linear",
			"resolution": "seconds",
			"points": [
				{ "timestamp": "1970-01-01T00:00:00Z", "value": 0.0 },
				{ "timestamp": "1970-01-01T00:00:10Z", "value": 10.0 },
			],
		});
		let builder = HttpRequest::builder().method(Method::POST).uri("/api/v1/interpolate").header(header::HOST, host).header(header::CONTENT_TYPE, "application/json");
		let builder = match origin {
			Some(origin) => builder.header(header::ORIGIN, origin),
			None => builder,
		};
		builder.body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()
	}

	async fn send(router: axum::Router, request: HttpRequest<Body>) -> (StatusCode, serde_json::Value) {
		let response = router.oneshot(request).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
	}

	#[tokio::test]
	async fn loopback_hosts_are_served() {
		let metrics = SharedMetrics::default();
		for host in ["localhost", "localhost:8080", "LocalHost:8080", "localhost:", "127.0.0.1", "127.0.0.1:8080", "127.10.20.30:1", "[::1]", "[::1]:8080", "[::ffff:127.0.0.1]:8080", "[::ffff:7f00:1]"] {
			let (status, body) = send(guarded(&metrics), get("/health", Some(host))).await;
			assert_eq!(status, StatusCode::OK, "Host {host:?}: {body}");
		}
	}

	/// Regression: a DNS-rebound request carries the attacker's domain in `Host`.
	#[tokio::test]
	async fn a_foreign_host_is_misdirected() {
		let metrics = SharedMetrics::default();
		for host in ["evil.example", "evil.example:8080", "localhost.evil.example", "127.0.0.1.nip.io", "10.0.0.1:8080", "0.0.0.0:8080", "[::2]:8080", "[::ffff:10.0.0.1]", "::1", "localhost:80a", "localhost:123456", "user@localhost", "localhost/x"] {
			for uri in ["/health", "/ready", "/metrics", "/api/v1/storage/stats"] {
				let (status, body) = send(guarded(&metrics), get(uri, Some(host))).await;
				assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "GET {uri} with Host {host:?}: {body}");
				assert!(body["error"].as_str().unwrap().contains(ALLOW_ANY_HOST_ENV), "the refusal names the escape hatch: {body}");
			}
		}
		// A state-changing request is refused the same way, before its handler runs.
		let (status, _) = send(guarded(&metrics), interpolate("evil.example:8080", None)).await;
		assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
		assert_eq!(metrics.snapshot().interpolate.requests, 0, "the handler never ran");
	}

	#[tokio::test]
	async fn a_missing_duplicated_or_foreign_target_host_is_misdirected() {
		let metrics = SharedMetrics::default();
		let (status, _) = send(guarded(&metrics), get("/health", None)).await;
		assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "no Host");
		let twice = HttpRequest::builder().uri("/health").header(header::HOST, "localhost").header(header::HOST, "evil.example").body(Body::empty()).unwrap();
		assert_eq!(send(guarded(&metrics), twice).await.0, StatusCode::MISDIRECTED_REQUEST, "two Host headers");
		let absolute = HttpRequest::builder().uri("http://evil.example/health").header(header::HOST, "localhost").body(Body::empty()).unwrap();
		assert_eq!(send(guarded(&metrics), absolute).await.0, StatusCode::MISDIRECTED_REQUEST, "a foreign absolute-form target");
		let absolute = HttpRequest::builder().uri("http://127.0.0.1:8080/health").body(Body::empty()).unwrap();
		assert_eq!(send(guarded(&metrics), absolute).await.0, StatusCode::OK, "a loopback absolute-form target without Host (HTTP/2 style)");
	}

	/// Regression: browsers attach `Origin` to a cross-origin `POST`, which a page can send
	/// to a loopback server without a preflight.
	#[tokio::test]
	async fn a_state_changing_request_from_a_foreign_origin_is_forbidden() {
		let metrics = SharedMetrics::default();
		for origin in ["https://evil.example", "http://evil.example:8080", "null", "http://localhost.evil.example", "http://127.0.0.1.nip.io:8080", "file://", "chrome-extension://abcdef", "http://localhost:8080/"] {
			let (status, body) = send(guarded(&metrics), interpolate("127.0.0.1:8080", Some(origin))).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "Origin {origin:?}: {body}");
			assert!(body["error"].as_str().unwrap().contains("web origin"), "{body}");
		}
		assert_eq!(metrics.snapshot().interpolate.requests, 0, "no refused request reached the handler");
		// Reads are left to the Host check and the browser's same-origin policy.
		let read = HttpRequest::builder().uri("/health").header(header::HOST, "localhost:8080").header(header::ORIGIN, "https://evil.example").body(Body::empty()).unwrap();
		assert_eq!(send(guarded(&metrics), read).await.0, StatusCode::OK);
	}

	#[tokio::test]
	async fn loopback_origins_and_clients_without_an_origin_may_post() {
		let metrics = SharedMetrics::default();
		for origin in [None, Some("http://localhost:8080"), Some("http://127.0.0.1:8080"), Some("https://[::1]:8443")] {
			let (status, body) = send(guarded(&metrics), interpolate("localhost:8080", origin)).await;
			assert_eq!(status, StatusCode::OK, "Origin {origin:?}: {body}");
		}
		assert_eq!(metrics.snapshot().interpolate.requests, 4);
	}

	#[tokio::test]
	async fn an_unguarded_router_serves_any_host_and_origin() {
		let router = || app_with_state(AppState::new());
		assert_eq!(send(router(), get("/health", Some("evil.example"))).await.0, StatusCode::OK);
		assert_eq!(send(router(), interpolate("evil.example", Some("https://evil.example"))).await.0, StatusCode::OK);
	}

	#[test]
	fn the_guard_follows_the_bound_address() {
		let at = |addr: &str| addr.parse::<SocketAddr>().unwrap();
		assert_eq!(HostGuard::for_bind(at("127.0.0.1:8080"), false), HostGuard::Loopback);
		assert_eq!(HostGuard::for_bind(at("127.1.2.3:8080"), false), HostGuard::Loopback);
		assert_eq!(HostGuard::for_bind(at("[::1]:8080"), false), HostGuard::Loopback);
		assert_eq!(HostGuard::for_bind(at("[::ffff:127.0.0.1]:8080"), false), HostGuard::Loopback, "an IPv4-mapped loopback bind is loopback");
		assert_eq!(HostGuard::for_bind(at("[::ffff:10.0.0.1]:8080"), false), HostGuard::Off);
		assert_eq!(HostGuard::for_bind(at("127.0.0.1:8080"), true), HostGuard::Off, "WEFT_ALLOW_ANY_HOST opts out");
		assert_eq!(HostGuard::for_bind(at("0.0.0.0:8080"), false), HostGuard::Off, "a non-loopback bind is unchanged");
		assert_eq!(HostGuard::for_bind(at("[::]:8080"), false), HostGuard::Off);
		assert_eq!(HostGuard::for_bind(at("192.168.1.10:8080"), false), HostGuard::Off);
		assert_eq!(AppState::new().host_guard(), HostGuard::Off, "a router is unguarded unless asked");
	}

	#[test]
	fn allow_any_host_reads_truthy_values() {
		for value in ["1", "true", "TRUE", " yes ", "on"] {
			assert!(allow_any_host_from_env(Some(value)), "{value:?}");
		}
		for value in [None, Some(""), Some("0"), Some("false"), Some("off"), Some("2")] {
			assert!(!allow_any_host_from_env(value), "{value:?}");
		}
	}
}
