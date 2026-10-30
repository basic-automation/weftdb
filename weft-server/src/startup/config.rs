//! Startup phase 2 — what the operator asked for: the bind address, the
//! interpolation cap, and (once the listener's address is known) the request guard.
//!
//! Every setting here is read from the environment exactly once, at startup. A
//! setting that is not a valid value stops the server from starting rather than
//! being silently defaulted: an operator's typo should fail loudly, at start.

use std::net::SocketAddr;

use weft_server::{
	host_guard::{self, HostGuard, ALLOW_ANY_HOST_ENV}, tls::{PlaintextGate, ALLOW_PLAINTEXT_ENV, AUTH_ENV, KEYS_FILE_ENV, TLS_CERT_FILE_ENV, TLS_KEY_FILE_ENV}, InterpolateConfig, ServeLimits, TlsConfig, HEADER_READ_TIMEOUT_ENV, KEEP_ALIVE_TIMEOUT_ENV, MAX_CONNECTIONS_ENV, MAX_INTERPOLATE_POINTS_ENV, WRITE_STALL_TIMEOUT_ENV
};

/// Default bind address when `WEFT_SERVER_ADDR` is unset.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// The address to bind, from `WEFT_SERVER_ADDR` (e.g. `0.0.0.0:9000`) or
/// [`DEFAULT_ADDR`].
///
/// # Errors
///
/// A value that is not a parseable socket address.
pub(crate) fn bind_addr() -> anyhow::Result<SocketAddr> {
	Ok(std::env::var("WEFT_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string()).parse()?)
}

/// Read the interpolation settings ([`InterpolateConfig`]) from
/// `WEFT_MAX_INTERPOLATE_POINTS`, once, and log the output-grid cap in force.
///
/// # Errors
///
/// A value that is not a positive integer (a malformed operator config should fail
/// loudly at start rather than silently default).
pub(crate) fn interpolate_config() -> anyhow::Result<InterpolateConfig> {
	let config = InterpolateConfig::from_env_value(std::env::var(MAX_INTERPOLATE_POINTS_ENV).ok().as_deref()).map_err(anyhow::Error::msg)?;
	println!("interpolation: at most {} output points per request (set {MAX_INTERPOLATE_POINTS_ENV} to change)", config.max_output_points());
	Ok(config)
}

/// Read the serve path's pre-auth listener limits ([`ServeLimits`], SEC-4)
/// from the environment, once, and log the limits in force. The connection
/// cap defaults to half the process's `RLIMIT_NOFILE` soft limit (see
/// `weft_server::serve`).
///
/// # Errors
///
/// A value that is not a positive integer (a malformed operator config should
/// fail loudly at start rather than silently default).
pub(crate) fn serve_limits() -> anyhow::Result<ServeLimits> {
	let limits = ServeLimits::from_env().map_err(anyhow::Error::msg)?;
	println!("listener limits: at most {} connections (set {MAX_CONNECTIONS_ENV} to change; the default is half the RLIMIT_NOFILE soft limit), header deadline {}s (set {HEADER_READ_TIMEOUT_ENV}), keep-alive deadline {}s (set {KEEP_ALIVE_TIMEOUT_ENV}), write-stall timeout {}s (set {WRITE_STALL_TIMEOUT_ENV})", limits.max_connections(), limits.header_read_timeout().as_secs(), limits.keep_alive_timeout().as_secs(), limits.write_stall_timeout().as_secs());
	Ok(limits)
}

/// Resolve the listener's transport for `addr` (SEC-5): TLS when a
/// certificate and key are configured, plaintext otherwise — and, for
/// plaintext, run the C8 gate ([`PlaintextGate::check`]) against the
/// address about to be bound, so a refused exposure stops startup before
/// anything is served. Logs which transport is in force and, for a
/// plaintext opt-in, which opt-in carried it.
///
/// # Errors
///
/// A half-configured TLS pair, TLS files that cannot be loaded, or the
/// gate's refusal of an unopted plaintext exposure.
pub(crate) fn transport(addr: SocketAddr) -> anyhow::Result<Option<TlsConfig>> {
	let tls = TlsConfig::from_env().map_err(anyhow::Error::msg)?;
	let gate = PlaintextGate::from_env();
	gate.check(addr, tls.as_ref()).map_err(anyhow::Error::msg)?;
	match &tls {
		Some(_) => println!("transport: TLS (rustls, ring provider; certificate {TLS_CERT_FILE_ENV}, key {TLS_KEY_FILE_ENV})"),
		None if addr.ip().to_canonical().is_loopback() => println!("transport: plaintext on loopback (the request guard applies; TLS is off)"),
		None if gate.auth_disabled => println!("transport: plaintext on {addr} — authentication is explicitly disabled ({AUTH_ENV}=disabled); a gateway in front of this server owns it"),
		None => println!("transport: plaintext on {addr} — keys file mounted ({KEYS_FILE_ENV}) and {ALLOW_PLAINTEXT_ENV} set; traffic is NOT encrypted"),
	}
	Ok(tls)
}

/// Pick the request guard for the bound address `local` ([`HostGuard::for_bind`],
/// honouring `WEFT_ALLOW_ANY_HOST`) and log which one is in force.
pub(crate) fn host_guard_for(local: SocketAddr) -> HostGuard {
	let allow_any_host = host_guard::allow_any_host_from_env(std::env::var(ALLOW_ANY_HOST_ENV).ok().as_deref());
	let guard = HostGuard::for_bind(local, allow_any_host);
	match guard {
		HostGuard::Loopback => println!("request guard: loopback only (Host must be localhost, 127.0.0.0/8 or [::1]; state-changing requests from non-loopback web origins are refused; set {ALLOW_ANY_HOST_ENV}=1 behind a local reverse proxy that rewrites Host or forwards a non-loopback Origin)"),
		HostGuard::Off if local.ip().to_canonical().is_loopback() => println!("request guard: off ({ALLOW_ANY_HOST_ENV} is set)"),
		HostGuard::Off => println!("request guard: off (bound to the non-loopback address {local}; there is no authentication, so keep this server off untrusted networks)"),
	}
	guard
}
