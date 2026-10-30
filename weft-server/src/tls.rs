//! TLS on the shared serve path, and the plaintext opt-in gate (SEC-5).
//!
//! SEC-4 built one serve path ([`crate::serve::run`]) and named this slice
//! as its second caller: every listener goes through that path, so the
//! pre-auth limits apply before any route, gate or auth code runs.
//! [`crate::serve::run_tls`] is that path with a rustls (ring) handshake in
//! front of each accepted connection — the handshake runs inside the
//! connection task, under the connection cap and the keep-alive deadline,
//! so an unauthenticated peer that stalls mid-handshake holds a cap slot
//! for at most that deadline, never an unbounded task.
//!
//! ## Configuration
//!
//! TLS is configured by naming both files: [`TLS_CERT_FILE_ENV`] (a PEM
//! certificate chain) and [`TLS_KEY_FILE_ENV`] (the matching PEM private
//! key). Naming exactly one is a startup error — half a TLS configuration
//! silently ignored would serve plaintext an operator believes is
//! encrypted. With neither named, the listener is plaintext and the gate
//! below decides whether that may start.
//!
//! ## The plaintext gate (plan §4 C8, decision 7)
//!
//! Plaintext on a **non-loopback** bind is an exposure, so it needs an
//! explicit opt-in, exactly one of:
//!
//! - a mounted keys file ([`KEYS_FILE_ENV`] names it and the file is
//!   there — SEC-6 defines its format and reads it; this slice only
//!   establishes that the deployment mounted one) **plus**
//!   [`ALLOW_PLAINTEXT_ENV`] acknowledging the traffic is unencrypted; or
//! - [`AUTH_ENV`]`=disabled`, the explicit statement that authentication
//!   is off because a gateway in front of the server owns it.
//!
//! Loopback binds are ungated: decision 7's protected mode serves loopback
//! with the Host/Origin guard and no keys. TLS binds are ungated: the
//! traffic is encrypted whatever the authentication story is. The full
//! exposure matrix (proxied-request refusal, the metrics scope, the
//! loopback guard's interplay with auth) is SEC-8's; this gate is only
//! the C8 refusal SEC-5's acceptance names.

use std::{
	fs, net::SocketAddr, path::{Path, PathBuf}, sync::Arc
};

use rustls::{
	crypto::ring, pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer}, ServerConfig
};
use tokio_rustls::TlsAcceptor;

/// Environment variable naming the PEM certificate chain file the TLS
/// listener presents: set together with [`TLS_KEY_FILE_ENV`], read once at
/// startup.
pub const TLS_CERT_FILE_ENV: &str = "WEFT_TLS_CERT_FILE";

/// Environment variable naming the PEM private-key file matching
/// [`TLS_CERT_FILE_ENV`]: set together with it, read once at startup.
pub const TLS_KEY_FILE_ENV: &str = "WEFT_TLS_KEY_FILE";

/// Environment variable naming the API keys file. SEC-6 defines its format
/// and enforces the keys; SEC-5's gate reads its *presence*: a mounted
/// keys file is one half of the plaintext opt-in on a non-loopback bind
/// (C8), because it is what the Docker image mounts.
pub const KEYS_FILE_ENV: &str = "WEFT_KEYS_FILE";

/// Environment variable that explicitly opts in to unencrypted traffic:
/// with a mounted keys file it lets a non-loopback bind serve plaintext
/// (C8). `1`/`true`/`yes`/`on` opts in; unset, `0` or anything else does
/// not.
pub const ALLOW_PLAINTEXT_ENV: &str = "WEFT_ALLOW_PLAINTEXT";

/// Environment variable carrying the authentication mode. The only value
/// this slice reads is [`AUTH_DISABLED`]: authentication explicitly off,
/// which on a non-loopback bind is the gateway deployment's plaintext
/// opt-in (C8, decision 7). SEC-6 owns the modes that turn auth on.
pub const AUTH_ENV: &str = "WEFT_AUTH";

/// The [`AUTH_ENV`] value that turns authentication explicitly off.
pub const AUTH_DISABLED: &str = "disabled";

/// The server's TLS configuration: a rustls [`ServerConfig`] built on the
/// ring provider, ready to accept connections on the shared serve path.
#[derive(Clone)]
pub struct TlsConfig {
	acceptor: TlsAcceptor,
}

impl std::fmt::Debug for TlsConfig {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("TlsConfig").finish_non_exhaustive()
	}
}

impl TlsConfig {
	/// Build the configuration from a PEM certificate chain and its PEM
	/// private key. The provider is ring, explicitly: no process-global
	/// provider is installed or consulted, so a library consumer's own
	/// rustls choices cannot change what this server negotiates. ALPN
	/// offers `http/1.1` only — HTTP/1.1 only is decision 17, and HTTP/2 is
	/// deferred past 1.0 by the plan (§10).
	///
	/// # Errors
	///
	/// A message naming what is wrong: unparseable PEM, an empty
	/// certificate chain, a missing private key, or a key that does not
	/// match the chain (rustls checks the pair).
	pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, String> {
		let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<_, _>>().map_err(|e| format!("{TLS_CERT_FILE_ENV}: the certificate file is not PEM certificates: {e}"))?;
		if chain.is_empty() {
			return Err(format!("{TLS_CERT_FILE_ENV}: the certificate file holds no PEM certificate"));
		}
		let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("{TLS_KEY_FILE_ENV}: the key file holds no PEM private key: {e}"))?;
		let provider = Arc::new(ring::default_provider());
		let mut config = ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions().map_err(|e| format!("rustls rejected its own protocol versions: {e}"))?.with_no_client_auth().with_single_cert(chain, key).map_err(|e| format!("the certificate chain and private key do not form a usable pair: {e}"))?;
		config.alpn_protocols = vec![b"http/1.1".to_vec()];
		Ok(Self { acceptor: TlsAcceptor::from(Arc::new(config)) })
	}

	/// Load the configuration from the two files an operator named.
	///
	/// # Errors
	///
	/// A file that cannot be read (the message names the variable and the
	/// path), or whatever [`from_pem`](Self::from_pem) rejects.
	pub fn load(cert_path: &Path, key_path: &Path) -> Result<Self, String> {
		let cert_pem = fs::read(cert_path).map_err(|e| format!("{TLS_CERT_FILE_ENV}={}: cannot read the certificate file: {e}", cert_path.display()))?;
		let key_pem = fs::read(key_path).map_err(|e| format!("{TLS_KEY_FILE_ENV}={}: cannot read the key file: {e}", key_path.display()))?;
		Self::from_pem(&cert_pem, &key_pem)
	}

	/// The configuration two environment values describe: both set loads
	/// TLS, neither set is plaintext (`Ok(None)`), and exactly one set is
	/// an error naming the missing half.
	///
	/// # Errors
	///
	/// One of the pair set without the other, or whatever
	/// [`load`](Self::load) rejects.
	pub fn from_env_values(cert_file: Option<&str>, key_file: Option<&str>) -> Result<Option<Self>, String> {
		match (cert_file, key_file) {
			(Some(cert), Some(key)) => Ok(Some(Self::load(Path::new(cert), Path::new(key))?)),
			(None, None) => Ok(None),
			(Some(_), None) => Err(format!("{TLS_CERT_FILE_ENV} is set but {TLS_KEY_FILE_ENV} is not: TLS needs the certificate and its private key together")),
			(None, Some(_)) => Err(format!("{TLS_KEY_FILE_ENV} is set but {TLS_CERT_FILE_ENV} is not: TLS needs the certificate and its private key together")),
		}
	}

	/// The configuration the process environment describes; see
	/// [`from_env_values`](Self::from_env_values).
	///
	/// # Errors
	///
	/// A half-configured pair, or files that cannot be loaded.
	pub fn from_env() -> Result<Option<Self>, String> {
		Self::from_env_values(std::env::var(TLS_CERT_FILE_ENV).ok().as_deref(), std::env::var(TLS_KEY_FILE_ENV).ok().as_deref())
	}

	/// The acceptor the serve path wraps each accepted connection with.
	#[must_use]
	pub fn acceptor(&self) -> TlsAcceptor {
		self.acceptor.clone()
	}
}

/// Whether a value opts in to plaintext: the same spellings the request
/// guard accepts for `WEFT_ALLOW_ANY_HOST` (`1`/`true`/`yes`/`on`,
/// trimmed, any case); unset, empty, `0` or anything else does not opt in.
#[must_use]
pub fn allow_plaintext_from_env(value: Option<&str>) -> bool {
	value.is_some_and(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Whether a value disables authentication explicitly: exactly
/// [`AUTH_DISABLED`], trimmed and in any case. Decision 7: the opt-out is
/// only ever explicit, so an unrecognised value is not read as one.
#[must_use]
pub fn auth_disabled_from_env(value: Option<&str>) -> bool {
	value.is_some_and(|raw| raw.trim().eq_ignore_ascii_case(AUTH_DISABLED))
}

/// The plaintext gate's inputs, read once at startup alongside the TLS
/// configuration (C8).
#[derive(Debug, Clone, Default)]
pub struct PlaintextGate {
	/// The keys file the deployment mounted, when [`KEYS_FILE_ENV`] names
	/// one — whether the file is actually there is the gate's question,
	/// answered at check time.
	pub keys_file: Option<PathBuf>,
	/// [`ALLOW_PLAINTEXT_ENV`] opted in.
	pub allow_plaintext: bool,
	/// [`AUTH_ENV`]`=disabled` was set explicitly.
	pub auth_disabled: bool,
}

impl PlaintextGate {
	/// The gate the three environment values describe.
	#[must_use]
	pub fn from_env_values(keys_file: Option<&str>, allow_plaintext: Option<&str>, auth_mode: Option<&str>) -> Self {
		Self { keys_file: keys_file.map(PathBuf::from), allow_plaintext: allow_plaintext_from_env(allow_plaintext), auth_disabled: auth_disabled_from_env(auth_mode) }
	}

	/// The gate the process environment describes.
	#[must_use]
	pub fn from_env() -> Self {
		Self::from_env_values(std::env::var(KEYS_FILE_ENV).ok().as_deref(), std::env::var(ALLOW_PLAINTEXT_ENV).ok().as_deref(), std::env::var(AUTH_ENV).ok().as_deref())
	}

	/// Whether the keys file is mounted: named, and a readable file. A
	/// named-but-absent file is reported as not mounted rather than
	/// erroring here, so the refusal message can say exactly that.
	#[must_use]
	pub fn keys_file_mounted(&self) -> bool {
		self.keys_file.as_ref().is_some_and(|path| path.is_file())
	}

	/// Decide whether a listener on `bind` may serve, given the TLS
	/// configuration startup resolved (`Some` = TLS). See the module docs
	/// for the rule; the error *is* the operator-facing refusal, naming
	/// every way to satisfy the gate.
	///
	/// # Errors
	///
	/// Plaintext on a non-loopback bind with no opt-in in force.
	pub fn check(&self, bind: SocketAddr, tls: Option<&TlsConfig>) -> Result<(), String> {
		if tls.is_some() || bind.ip().to_canonical().is_loopback() {
			return Ok(());
		}
		if self.auth_disabled {
			return Ok(());
		}
		if self.allow_plaintext && self.keys_file_mounted() {
			return Ok(());
		}
		let keys = match &self.keys_file {
			Some(path) if self.allow_plaintext => format!("{KEYS_FILE_ENV}={} does not name a mounted file", path.display()),
			Some(path) => format!("{KEYS_FILE_ENV}={} is mounted but {ALLOW_PLAINTEXT_ENV} is not set", path.display()),
			None if self.allow_plaintext => format!("{ALLOW_PLAINTEXT_ENV} is set but no keys file is mounted ({KEYS_FILE_ENV} is unset)"),
			None => format!("no keys file is mounted ({KEYS_FILE_ENV} is unset) and {ALLOW_PLAINTEXT_ENV} is not set"),
		};
		Err(format!("refusing to serve plaintext on the non-loopback address {bind}: {keys}. Serve TLS instead ({TLS_CERT_FILE_ENV} + {TLS_KEY_FILE_ENV}), or set {AUTH_ENV}=disabled when a gateway in front of this server owns authentication (plan C8)"))
	}
}

#[cfg(test)]
mod tests {
	use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

	use super::*;

	fn bind(ip: IpAddr) -> SocketAddr {
		SocketAddr::new(ip, 8080)
	}

	const NON_LOOPBACK: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);

	#[test]
	fn loopback_plaintext_needs_no_opt_in() {
		let gate = PlaintextGate::default();
		assert!(gate.check(bind(IpAddr::V4(Ipv4Addr::LOCALHOST)), None).is_ok());
		assert!(gate.check(bind(IpAddr::V6(Ipv6Addr::LOCALHOST)), None).is_ok());
	}

	#[test]
	fn non_loopback_plaintext_is_refused_without_an_opt_in() {
		let gate = PlaintextGate::default();
		let err = gate.check(bind(IpAddr::V4(NON_LOOPBACK)), None).expect_err("refused");
		assert!(err.contains("refusing to serve plaintext"), "{err}");
		assert!(err.contains(AUTH_ENV), "{err} names the gateway opt-in");
	}

	#[test]
	fn tls_on_a_non_loopback_bind_needs_no_opt_in() {
		// A configuration from generated material, so the check runs
		// against a real TlsConfig rather than a token stand-in.
		let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate");
		let tls = TlsConfig::from_pem(certified.cert.pem().as_bytes(), certified.signing_key.serialize_pem().as_bytes()).expect("config");
		let gate = PlaintextGate::default();
		assert!(gate.check(bind(IpAddr::V4(NON_LOOPBACK)), Some(&tls)).is_ok());
	}

	#[test]
	fn auth_disabled_is_a_plaintext_opt_in() {
		let gate = PlaintextGate { auth_disabled: true, ..PlaintextGate::default() };
		assert!(gate.check(bind(IpAddr::V4(NON_LOOPBACK)), None).is_ok());
		assert!(auth_disabled_from_env(Some("disabled")));
		assert!(auth_disabled_from_env(Some(" Disabled ")));
		for other in [None, Some(""), Some("on"), Some("enabled"), Some("keys")] {
			assert!(!auth_disabled_from_env(other), "{other:?} does not disable auth");
		}
	}

	#[test]
	fn plaintext_with_keys_needs_both_halves() {
		let dir = tempfile::tempdir().expect("tempdir");
		let keys = dir.path().join("keys.json");
		fs::write(&keys, b"{}").expect("write keys file");

		// Both halves: mounted keys + explicit plaintext opt-in.
		let both = PlaintextGate { keys_file: Some(keys.clone()), allow_plaintext: true, ..PlaintextGate::default() };
		assert!(both.check(bind(IpAddr::V4(NON_LOOPBACK)), None).is_ok());

		// Keys without the opt-in: refused, saying which half is missing.
		let keys_only = PlaintextGate { keys_file: Some(keys.clone()), ..PlaintextGate::default() };
		let err = keys_only.check(bind(IpAddr::V4(NON_LOOPBACK)), None).expect_err("refused");
		assert!(err.contains(ALLOW_PLAINTEXT_ENV), "{err}");

		// The opt-in without a mounted file: refused.
		let opt_in_only = PlaintextGate { keys_file: Some(dir.path().join("absent.json")), allow_plaintext: true, ..PlaintextGate::default() };
		assert!(opt_in_only.check(bind(IpAddr::V4(NON_LOOPBACK)), None).is_err());
		let unnamed = PlaintextGate { allow_plaintext: true, ..PlaintextGate::default() };
		let err = unnamed.check(bind(IpAddr::V4(NON_LOOPBACK)), None).expect_err("refused");
		assert!(err.contains(KEYS_FILE_ENV), "{err}");
	}

	#[test]
	fn half_a_tls_configuration_is_an_error() {
		for (cert, key) in [(Some("cert.pem"), None), (None, Some("key.pem"))] {
			let err = TlsConfig::from_env_values(cert, key).expect_err("half a pair is refused");
			assert!(err.contains(TLS_CERT_FILE_ENV) && err.contains(TLS_KEY_FILE_ENV), "{err}");
		}
		assert!(TlsConfig::from_env_values(None, None).expect("neither is plaintext").is_none());
	}

	#[test]
	fn plaintext_spellings_match_the_request_guards() {
		for yes in ["1", "true", "YES", " on "] {
			assert!(allow_plaintext_from_env(Some(yes)), "{yes:?} opts in");
		}
		for no in [None, Some(""), Some("0"), Some("false"), Some("off"), Some("2")] {
			assert!(!allow_plaintext_from_env(no), "{no:?} does not opt in");
		}
	}

	#[test]
	fn bad_tls_material_is_refused_with_the_reason() {
		assert!(TlsConfig::from_pem(b"not pem", b"not pem").is_err(), "garbage is refused");
		assert!(TlsConfig::from_pem(b"", b"").is_err(), "empty is refused");
		let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate");
		let other = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate");
		// One certificate with the other's key: rustls checks the pair.
		assert!(TlsConfig::from_pem(certified.cert.pem().as_bytes(), other.signing_key.serialize_pem().as_bytes()).is_err(), "a mismatched pair is refused");
	}
}
