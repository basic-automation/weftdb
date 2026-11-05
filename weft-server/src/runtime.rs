//! The process runtime (1.0 plan **ROB-11**, "Admission II" — the
//! runtime-builder side).
//!
//! Admission I (ROB-10, [`crate::admission`]) lives in the registry's
//! gate: it decides whether a matched request runs at all, by class.
//! Admission II is the other side of the same question — what admitted
//! work runs *on*. Until this slice the binary's runtime was whatever
//! `#[tokio::main]` built implicitly: its worker count, its thread
//! names and its shutdown behaviour were the attribute's defaults,
//! invisible in the tree and not the operator's to tune, while the
//! gate's caps beside it (`WEFT_QUERY_MAX_INFLIGHT`, the ingest byte
//! budget) are explicit registry knobs read once at startup. The
//! L-MAIN lane reserves this wave's second main.rs edit for exactly
//! this (plan §2: "PAC-7 → ROB-11 (W6, CLI then runtime builder)").
//!
//! Now the binary builds its runtime here, explicitly:
//!
//! - a multi-thread runtime from [`tokio::runtime::Builder`], with
//!   every driver enabled (I/O and time, as `#[tokio::main]` did);
//! - worker threads named `weft-worker`, so a thread dump or a
//!   debugger shows the server's pool as itself rather than as a set
//!   of anonymous tokio workers;
//! - the worker count is the operator's: [`WORKER_THREADS_ENV`]
//!   (`WEFT_WORKER_THREADS`, a Tuning entry in the VarSpec registry,
//!   [`crate::settings`]) when set, and the runtime's own default —
//!   one worker per available core — when it is not. The knob sits
//!   beside Admission I's caps on purpose: sizing the pool and sizing
//!   the classes' budgets are the two halves of admitting load, and
//!   they are tuned together;
//! - and when the process's work is done the runtime is shut down
//!   with [`RUNTIME_SHUTDOWN_TIMEOUT`]: plan C9's drain order ends
//!   "serve → SEC-9 TaskTracker → `store.shutdown()` → runtime
//!   `shutdown_timeout(5 s)`", and the first three steps happen inside
//!   startup. The implicit runtime had no such bound — dropping it
//!   waits indefinitely for blocking work to finish, so a wedged
//!   blocking task could hold a stopped process open forever. Five
//!   seconds after the store has shut down, whatever background work
//!   remains is abandoned and the process exits.
//!
//! This module owns no admission *policy*: the gate's classes,
//! budgets and refusals are Admission I's and are unchanged. Nothing
//! here queues, prioritises or overrides a route — the runtime side
//! only makes the capacity those decisions spend explicit, sized and
//! bounded at shutdown.

use std::{future::Future, time::Duration};

/// Environment variable naming the runtime's worker-thread count
/// (ROB-11): a positive whole number of threads, read once by `main`
/// before the runtime is built.
///
/// Registered in the `VarSpec` registry
/// ([`crate::settings`]).
pub const WORKER_THREADS_ENV: &str = "WEFT_WORKER_THREADS";

/// The name every runtime worker thread carries (ROB-11), so the
/// server's pool is recognisable in a thread dump.
pub const WORKER_THREAD_NAME: &str = "weft-worker";

/// How long the runtime waits for background work to finish once the
/// process's own work is done, before the remaining tasks are
/// abandoned and the process exits.
///
/// The last step of plan C9's drain
/// order ("runtime `shutdown_timeout(5 s)`").
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The runtime's shape (ROB-11), read once by `main` before anything
/// async runs.
///
/// The runtime must exist before the startup phases
/// (strict configuration included) can execute, so a malformed value
/// is refused by `main` itself, with the plan C7 configuration status
/// (78); the `VarSpec` registry's strict check would report the same
/// value in the same words had it been able to run first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
	worker_threads: Option<usize>,
}

impl RuntimeConfig {
	/// The defaults: no explicit worker count, so the runtime sizes
	/// its pool from the machine (one worker per available core) —
	/// the behaviour `#[tokio::main]` had.
	pub const DEFAULT: Self = Self { worker_threads: None };

	/// A configuration with an explicit worker-thread count (tests,
	/// embedders).
	#[must_use]
	pub const fn with_worker_threads(worker_threads: usize) -> Self {
		Self { worker_threads: Some(worker_threads) }
	}

	/// The configuration the `WEFT_WORKER_THREADS` value describes
	/// (`None` for an unset variable, which leaves the default).
	///
	/// # Errors
	///
	/// A message naming the variable when the value is not a positive
	/// whole number, so a malformed setting fails before the runtime
	/// is built instead of silently sizing the pool by a default the
	/// operator did not ask for.
	pub fn from_env_values(worker_threads: Option<&str>) -> Result<Self, String> {
		let mut config = Self::DEFAULT;
		if let Some(raw) = worker_threads {
			let parsed: usize = raw.parse().map_err(|e| format!("{WORKER_THREADS_ENV}={raw:?} is not a positive integer number of worker threads: {e}"))?;
			if parsed == 0 {
				return Err(format!("{WORKER_THREADS_ENV}={raw:?} is not a positive integer number of worker threads: zero would leave no thread to serve on"));
			}
			config.worker_threads = Some(parsed);
		}
		Ok(config)
	}

	/// The configuration the process environment describes; see
	/// [`from_env_values`](Self::from_env_values).
	///
	/// # Errors
	///
	/// A set variable whose value is not a positive whole number.
	pub fn from_env() -> Result<Self, String> {
		Self::from_env_values(std::env::var(WORKER_THREADS_ENV).ok().as_deref())
	}

	/// The explicit worker-thread count, or [`None`] when the
	/// runtime sizes its own pool from the machine.
	#[must_use]
	pub const fn worker_threads(self) -> Option<usize> {
		self.worker_threads
	}
}

impl Default for RuntimeConfig {
	fn default() -> Self {
		Self::DEFAULT
	}
}

/// Build the process runtime [`run`] drives.
///
/// Multi-thread, every
/// driver enabled, workers named [`WORKER_THREAD_NAME`], and as many
/// of them as `config` asks for (the runtime's machine-sized default
/// when it asks for none).
///
/// # Errors
///
/// Whatever keeps the runtime from being built (tokio reports a
/// failure to spawn its worker threads as an I/O error).
pub fn build_runtime(config: RuntimeConfig) -> std::io::Result<tokio::runtime::Runtime> {
	let mut builder = tokio::runtime::Builder::new_multi_thread();
	builder.enable_all().thread_name(WORKER_THREAD_NAME);
	if let Some(worker_threads) = config.worker_threads() {
		builder.worker_threads(worker_threads);
	}
	builder.build()
}

/// Run `future` to completion on a runtime built from `config`, then
/// shut the runtime down with [`RUNTIME_SHUTDOWN_TIMEOUT`].
///
/// Plan
/// C9's final drain step, bounding how long background work that
/// outlived the serve path can hold the process open.
///
/// # Errors
///
/// Whatever [`build_runtime`] reports; the future's own output is
/// returned untouched once the runtime exists.
pub fn run<F: Future>(config: RuntimeConfig, future: F) -> std::io::Result<F::Output> {
	let runtime = build_runtime(config)?;
	let output = runtime.block_on(future);
	runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
	Ok(output)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn runtime_config_parses_and_refuses_zero() {
		let config = RuntimeConfig::from_env_values(Some("4")).expect("valid");
		assert_eq!(config.worker_threads(), Some(4));
		assert_eq!(RuntimeConfig::from_env_values(None).expect("defaults"), RuntimeConfig::DEFAULT);
		assert_eq!(RuntimeConfig::DEFAULT.worker_threads(), None);
		assert!(RuntimeConfig::from_env_values(Some("0")).is_err());
		assert!(RuntimeConfig::from_env_values(Some("many")).is_err());
	}

	#[test]
	fn the_shutdown_timeout_is_c9s_five_seconds() {
		assert_eq!(RUNTIME_SHUTDOWN_TIMEOUT, Duration::from_secs(5));
	}

	#[test]
	fn run_drives_the_future_on_the_built_runtime() {
		// The future spawns (so the runtime must be multi-thread with
		// its drivers enabled, not a current-thread stand-in) and the
		// spawned task observes the worker pool's thread name.
		let output = run(RuntimeConfig::with_worker_threads(2), async { tokio::spawn(async { std::thread::current().name().map(str::to_owned) }).await.expect("the spawned task runs") }).expect("the runtime builds");
		assert_eq!(output.as_deref(), Some(WORKER_THREAD_NAME));
	}

	#[test]
	fn run_returns_the_futures_output_under_the_default_config() {
		let output = run(RuntimeConfig::DEFAULT, async { 40 + 2 }).expect("the runtime builds");
		assert_eq!(output, 42);
	}
}
