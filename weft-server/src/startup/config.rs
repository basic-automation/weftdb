//! Startup phase 2 — what the operator asked for: the bind address, the
//! interpolation cap, and (once the listener's address is known) the request guard.
//!
//! Every setting here is read from the environment exactly once, at startup. A
//! setting that is not a valid value stops the server from starting rather than
//! being silently defaulted: an operator's typo should fail loudly, at start.

use std::net::SocketAddr;

use weft_server::{
	host_guard::{self, HostGuard, ALLOW_ANY_HOST_ENV}, InterpolateConfig, MAX_INTERPOLATE_POINTS_ENV
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
