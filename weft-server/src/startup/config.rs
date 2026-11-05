//! Startup phase 2 — what the operator asked for: the bind address, the
//! interpolation cap, and (once the listener's address is known) the request guard.
//!
//! Every setting here is read from the environment exactly once, at startup. A
//! setting that is not a valid value stops the server from starting rather than
//! being silently defaulted: an operator's typo should fail loudly, at start.

use std::net::SocketAddr;

use weft_server::{
	auth::AuthConfig, host_guard::{self, HostGuard, ALLOW_ANY_HOST_ENV}, tls::{PlaintextGate, ALLOW_PLAINTEXT_ENV, AUTH_ENV, KEYS_FILE_ENV, TLS_CERT_FILE_ENV, TLS_KEY_FILE_ENV}, AdmissionConfig, IngestConfig, InterpolateConfig, ReadLimits, ServeLimits, TlsConfig, AUTH_KEYS_MODE, DEFAULT_PAGE_ROWS, HEADER_READ_TIMEOUT_ENV, INGEST_MAX_INFLIGHT_BYTES_ENV, KEEP_ALIVE_TIMEOUT_ENV, MAX_AT_MULTI_INSTANTS_ENV, MAX_CONNECTIONS_ENV, MAX_DOWNSAMPLE_BUCKETS_ENV, MAX_INGEST_BYTES_ENV, MAX_INTERPOLATE_POINTS_ENV, MAX_PAGE_ROWS_ENV, MAX_READ_ROWS_ENV, QUERY_MAX_INFLIGHT_ENV, READ_TIMEOUT_SECS_ENV, SHUTDOWN_GRACE_SECS_ENV, WRITE_STALL_TIMEOUT_ENV
};

/// Default bind address when `WEFT_SERVER_ADDR` is unset.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// The environment variable naming the probe-only admin listener's address
/// (ROB-23r). Unset is the default and means no second listener at all.
pub(crate) const ADMIN_ADDR_ENV: &str = "WEFT_ADMIN_ADDR";

/// The address to bind, from `WEFT_SERVER_ADDR` (e.g. `0.0.0.0:9000`) or
/// [`DEFAULT_ADDR`].
///
/// # Errors
///
/// A value that is not a parseable socket address.
pub fn bind_addr() -> anyhow::Result<SocketAddr> {
	Ok(std::env::var("WEFT_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string()).parse()?)
}

/// The address the probe-only admin listener binds (ROB-23r), from
/// [`ADMIN_ADDR_ENV`]. Unset — the default, and how every deployment running
/// today is configured — means there is no second listener and the probes stay
/// on the ordinary address.
///
/// A configured address is put through the same C8 plaintext gate as the data
/// plane, so a probe listener bound to a routable address still needs
/// `WEFT_ALLOW_PLAINTEXT` (or `WEFT_AUTH=disabled`) to start without TLS: a
/// listener that "only" answers `/health` is still a listener on the network.
///
/// # Errors
///
/// A value that is not a parseable socket address, or a plaintext exposure the
/// C8 gate refuses.
pub(crate) fn admin_addr() -> anyhow::Result<Option<SocketAddr>> {
	let Some(raw) = std::env::var(ADMIN_ADDR_ENV).ok().filter(|value| !value.trim().is_empty()) else {
		return Ok(None);
	};
	let addr: SocketAddr = raw.parse()?;
	PlaintextGate::from_env().check(addr, None).map_err(anyhow::Error::msg)?;
	println!("admin listener: probe-only surface (/health, /ready) on http://{addr}");
	Ok(Some(addr))
}

/// Read the interpolation settings ([`InterpolateConfig`]) from
/// `WEFT_MAX_INTERPOLATE_POINTS`, once, and log the output-grid cap in force.
///
/// # Errors
///
/// A value that is not a positive integer (a malformed operator config should fail
/// loudly at start rather than silently default).
pub fn interpolate_config() -> anyhow::Result<InterpolateConfig> {
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
pub fn serve_limits() -> anyhow::Result<ServeLimits> {
	let limits = ServeLimits::from_env().map_err(anyhow::Error::msg)?;
	println!("listener limits: at most {} connections (set {MAX_CONNECTIONS_ENV} to change; the default is half the RLIMIT_NOFILE soft limit), header deadline {}s (set {HEADER_READ_TIMEOUT_ENV}), keep-alive deadline {}s (set {KEEP_ALIVE_TIMEOUT_ENV}), write-stall timeout {}s (set {WRITE_STALL_TIMEOUT_ENV}), shutdown grace {}s (set {SHUTDOWN_GRACE_SECS_ENV})", limits.max_connections(), limits.header_read_timeout().as_secs(), limits.keep_alive_timeout().as_secs(), limits.write_stall_timeout().as_secs(), limits.shutdown_grace().as_secs());
	Ok(limits)
}

/// Read the ingest wiring's limits ([`IngestConfig`], D-S14) from the
/// environment, once, and log the limits in force: the one body cap for
/// the ingest routes and their in-flight byte budget.
///
/// # Errors
///
/// A value that is not a positive integer (a malformed operator config
/// should fail loudly at start rather than silently default).
pub fn ingest_config() -> anyhow::Result<IngestConfig> {
	let config = IngestConfig::from_env().map_err(anyhow::Error::msg)?;
	println!("ingest limits: body cap {} bytes (set {MAX_INGEST_BYTES_ENV} to change), in-flight budget {} bytes (set {INGEST_MAX_INFLIGHT_BYTES_ENV})", config.max_body_bytes(), config.max_inflight_bytes());
	Ok(config)
}

/// Read Admission I's limits ([`AdmissionConfig`], ROB-10) from the
/// environment, once, and log the cap in force: the query class's
/// in-flight cap the registry's gate enforces. (The write class's
/// budget is the ingest wiring's, above; probes are never shed and
/// maintenance is exclusive, so neither takes a setting.)
///
/// # Errors
///
/// A value that is not a positive integer (a malformed operator config
/// should fail loudly at start rather than silently default).
pub fn admission_config() -> anyhow::Result<AdmissionConfig> {
	let config = AdmissionConfig::from_env().map_err(anyhow::Error::msg)?;
	println!("admission limits: at most {} query-class requests in flight (set {QUERY_MAX_INFLIGHT_ENV} to change); probes are never shed, maintenance is admitted one at a time", config.max_query_inflight());
	Ok(config)
}

/// Read the read-path caps ([`ReadLimits`], ROB-12) from the environment,
/// once, and log the caps in force: the largest page, the largest result,
/// the largest downsample grid, the largest multi-instant list, and the
/// deadline one read gets. Every read refuses what cannot fit them — with
/// `422 result_too_large` for a page or a result over a cap, and `504
/// query_timeout` when it runs past the deadline — rather than attempting
/// an answer no client could hold.
///
/// # Errors
///
/// A value that is not a positive integer (a malformed operator config
/// should fail loudly at start rather than silently default).
pub(crate) fn read_limits() -> anyhow::Result<ReadLimits> {
	let limits = ReadLimits::from_env().map_err(anyhow::Error::msg)?;
	println!("read limits: a {DEFAULT_PAGE_ROWS}-row page by default and at most {} to a page (set {MAX_PAGE_ROWS_ENV}), at most {} rows to a read (set {MAX_READ_ROWS_ENV}), at most {} buckets to a downsample (set {MAX_DOWNSAMPLE_BUCKETS_ENV}), at most {} instants to a multi lookup (set {MAX_AT_MULTI_INSTANTS_ENV}), {} s to a read (set {READ_TIMEOUT_SECS_ENV})", limits.page_max(), limits.read_rows_max(), limits.downsample_buckets_max(), limits.at_multi_instants_max(), limits.read_timeout_secs());
	Ok(limits)
}

/// Resolve the listener's transport for `addr` (SEC-5): TLS when a
/// certificate and key are configured, plaintext otherwise — and run
/// the exposure matrix ([`PlaintextGate::check`], C8 as completed by
/// SEC-8) against the address about to be bound, so a refused exposure
/// stops startup before anything is served. Logs which transport is in
/// force and, for a plaintext opt-in, which opt-in carried it.
///
/// # Errors
///
/// A half-configured TLS pair, TLS files that cannot be loaded, or the
/// gate's refusal of an unopted plaintext exposure.
pub fn transport(addr: SocketAddr) -> anyhow::Result<Option<TlsConfig>> {
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

/// Resolve the Bearer authentication in force (SEC-6, [`AuthConfig`]):
/// the keys file `WEFT_KEYS_FILE` names is loaded here, so a
/// malformed keys file stops startup before anything is served — a
/// server that cannot load its keys must not come up unauthenticated
/// by accident. Later edits reload on SIGHUP (SEC-7; the watcher is
/// registered in the daemons phase). Logs which mode is in force.
///
/// # Errors
///
/// An unrecognised `WEFT_AUTH` value, `WEFT_AUTH=keys` with no keys
/// file mounted, or a keys file that cannot be loaded.
pub fn auth() -> anyhow::Result<AuthConfig> {
	let auth = AuthConfig::from_env().map_err(anyhow::Error::msg)?;
	match &auth {
		AuthConfig::Keys(store) => {
			let from = store.path().map_or_else(|| KEYS_FILE_ENV.to_string(), |path| format!("{}={}", KEYS_FILE_ENV, path.display()));
			println!("auth: Bearer API keys from {from} ({} key(s)); each key's role in the file grants its scopes on every route (SEC-6); SIGHUP reloads the file without a restart (SEC-7)", store.len());
		}
		AuthConfig::Disabled => println!("auth: explicitly disabled ({AUTH_ENV}=disabled); a gateway in front of this server owns authentication, and proxied requests are served"),
		AuthConfig::Off => println!("auth: off (no keys file mounted; set {KEYS_FILE_ENV} to a keys file, or {AUTH_ENV}={AUTH_KEYS_MODE} to require one); proxied requests are refused while auth is off (SEC-8)"),
	}
	Ok(auth)
}

/// Pick the request guard for the bound address `local` ([`HostGuard::for_bind`],
/// honouring `WEFT_ALLOW_ANY_HOST`) and log which one is in force.
pub fn host_guard_for(local: SocketAddr) -> HostGuard {
	let allow_any_host = host_guard::allow_any_host_from_env(std::env::var(ALLOW_ANY_HOST_ENV).ok().as_deref());
	let guard = HostGuard::for_bind(local, allow_any_host);
	match guard {
		HostGuard::Loopback => println!("request guard: loopback only (Host must be localhost, 127.0.0.0/8 or [::1]; state-changing requests from non-loopback web origins are refused; set {ALLOW_ANY_HOST_ENV}=1 behind a local reverse proxy that rewrites Host or forwards a non-loopback Origin)"),
		HostGuard::Off if local.ip().to_canonical().is_loopback() => println!("request guard: off ({ALLOW_ANY_HOST_ENV} is set)"),
		HostGuard::Off => println!("request guard: off (bound to the non-loopback address {local}; the guard is a loopback stop-gap, so this bind relies on authentication and the network it sits on)"),
	}
	guard
}

/// Read the Arrow Flight listener's address from `WEFT_FLIGHT_ADDR`, once: `None` leaves
/// Flight off.
///
/// # Errors
///
/// A value that is not a socket address, or one that is not loopback (the listener is
/// plaintext; [`weft_server::flight::flight_addr`] says why).
/// Phase 5 (5.7): the GPU adapter and device mode `WEFT_GPU_ADAPTER` / `WEFT_GPU_DEVICES`
/// ask for, or `None` when neither is set (splimes' single-device default). A malformed
/// value refuses the start (exit 78) rather than silently running on another device.
pub fn gpu_config() -> anyhow::Result<Option<splimes::GpuConfig>> {
	use weft_server::gpu::{gpu_config_from_env, GPU_ADAPTER_ENV, GPU_DEVICES_ENV};
	gpu_config_from_env(std::env::var(GPU_ADAPTER_ENV).ok().as_deref(), std::env::var(GPU_DEVICES_ENV).ok().as_deref()).map_err(anyhow::Error::msg)
}

pub fn flight_addr() -> anyhow::Result<Option<SocketAddr>> {
	weft_server::flight::flight_addr(std::env::var(weft_server::flight::FLIGHT_ADDR_ENV).ok().as_deref()).map_err(anyhow::Error::msg)
}
