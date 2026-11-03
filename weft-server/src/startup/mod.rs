//! The startup phases of the `weft-server` binary, in the order they run.
//!
//! The entry point ([`main`](crate)) only installs the runtime; what happens
//! after that is this list. Each phase is one module, so a later wave edits a
//! startup behaviour by editing that module rather than by growing a single file.
//!
//! The order matters, and since SEC-8 it is enforced, not just
//! documented: [`PHASES`] names every phase in order, and a test in this
//! module fails if `run`'s calls drift out of that order. The phases:
//!
//! 0. **strict-config** — PAC-5b's environment check, before any phase
//!    reads a setting (see the last paragraph below).
//! 1. [`hooks`] — the tracing subscriber and the OTLP exporter when configured, then the
//!    panic hook that ends the process when a control-plane write panics.
//!    Installed first so every later phase is observable.
//! 2. **signals** — the shutdown signal handlers (C9's "handlers first").
//! 3. [`config`] — what the operator asked for: bind address, interpolation cap,
//!    the transport and the exposure matrix's verdict on it, the keys
//!    file, and (once the address is known) the request guard. The
//!    exposure and auth resolutions run here, before the store opens:
//!    a refused exposure starts nothing at all.
//! 4. [`state`] — the router state, which means opening the segment store when
//!    one is configured.
//! 5. [`daemons`] — the background reconcile/backup/GPU-calibration tasks,
//!    registered in the Daemons registry (ROB-3), which supervises them and
//!    stops them on its shared shutdown watch when serving ends.
//! 6. [`serve`] — bind, print the banner, serve until shutdown.
//!
//! The shutdown signal handlers are installed before the config phase (right after
//! [`hooks`], whose tracing they report through): plan C9's "handlers first".
//! A signal that arrives at any point after that — including while the store
//! is still opening, before serving starts — is recorded, and the serve phase
//! sees it on entry and exits 0 without accepting a connection.
//!
//! ## Request guard
//!
//! When the bound address is a loopback address, every request must be addressed to a
//! loopback host (`Host: localhost`, `127.0.0.0/8` or `[::1]`, else `421`), and a
//! state-changing request carrying a non-loopback `Origin` is refused (`403`), so web
//! pages from non-loopback origins, open in a local browser, cannot drive the server
//! ([`weft_server::host_guard`]).
//! `WEFT_ALLOW_ANY_HOST=1` turns this off for a local reverse proxy that forwards a
//! different `Host` or a browser UI's non-loopback `Origin`. A non-loopback bind is not guarded by it:
//! the guard is a loopback stop-gap, and authentication (below) is what a
//! non-loopback deployment relies on.
//!
//! ## Transport (SEC-5) and the exposure matrix (SEC-8)
//!
//! With `WEFT_TLS_CERT_FILE` and `WEFT_TLS_KEY_FILE` set, the listener speaks
//! TLS (rustls, ring provider) on the same serve path; the banner says
//! `https`. Without them the listener is plaintext. Either way, a
//! non-loopback bind must pass the exposure matrix (C8, decision 7)
//! before anything is served: a mounted keys file (`WEFT_KEYS_FILE`) —
//! plus `WEFT_ALLOW_PLAINTEXT=1` when the transport is plaintext — or
//! `WEFT_AUTH=disabled` behind a gateway. Non-loopback with no keys and
//! no gateway statement refuses to start, TLS included; loopback is
//! ungated at startup. See [`weft_server::tls`].
//!
//! ## Authentication (SEC-6) and the auth-off guard (SEC-8)
//!
//! With `WEFT_KEYS_FILE` naming a keys file (or `WEFT_AUTH=keys`, which
//! requires one), every request is authenticated with
//! `Authorization: Bearer <key>` against the file's key hashes, and the
//! key's role is checked against the scope the sealed registry assigns
//! the route ([`weft_server::auth`]) — `/metrics` included, under the
//! registry's `metrics` scope (decision 17). The probes stay open to the
//! anonymous principal; everything else needs a key. `WEFT_AUTH=disabled`
//! turns authentication explicitly off for the gateway deployment, where
//! proxied requests are expected. With no keys file mounted the server
//! runs unauthenticated, as it did before SEC-6 — but a request that
//! arrives through a proxy is refused (`403 proxied_request_requires_auth`),
//! because a proxy in the path breaks the loopback assumption the
//! keyless mode rests on. `weft-server keygen` mints a key and records
//! its hash in the file. The packaging track's `WEFT_ALLOW_INSECURE_BIND`
//! is dropped: it is not a registered variable, so strict configuration
//! refuses it as unknown; `WEFT_AUTH=disabled` is the spelling.
//!
//! ## Optional segment store
//!
//! Set `WEFT_SEGMENT_STORE_ROOT` to a directory to open a
//! [`SegmentStore`](weftdb::SegmentStore) rooted there (creating the Storage v2 layout
//! if absent). With it set, the stored-range query endpoints (`/api/v1/storage/...`) go
//! live; without it the server serves only the stateless interpolation/downsample API
//! and those endpoints answer `503`. Readiness (`GET /ready`) reports which mode is
//! active.
//!
//! The store is opened with the options the `WEFT_SEGMENT_*` variables describe
//! ([`weft_server::store_open`]); the library reads none of them itself. With
//! `WEFT_ON_AMBIGUOUS_COMMIT=exit` the server exits with status 70 once the store is
//! write-poisoned.
//!
//! ## Interpolation cap
//!
//! Every `/api/v1/interpolate*` request may produce at most 10,000,000 output points
//! (`weft_server::MAX_INTERPOLATE_OUTPUT_POINTS`); `WEFT_MAX_INTERPOLATE_POINTS` sets
//! another positive limit. It is read once at startup, and a value that is not a
//! positive integer stops the server from starting.
//!
//! ## GPU calibration
//!
//! Once the listener is bound, the server calibrates the interpolation engine's backend
//! choice once, in the background ([`weft_server::gpu`]): it starts the GPU if there is
//! one and measures where the rayon pool and the GPU overtake a single core. That takes
//! several seconds, during which requests are already served and interpolate on the CPU
//! with splimes' default thresholds. A CPU/software adapter (llvmpipe, lavapipe, WARP) is
//! not calibrated unless `WEFT_GPU_CALIBRATE=force`; `WEFT_GPU_CALIBRATE=0` skips
//! calibration altogether. It never stops the server from starting.
//! Before the first phase, strict configuration (PAC-5b) checks the environment against
//! the registry of every variable the workspace reads: a `WEFT_` name no crate reads, a
//! test harness's own channel set on a real server, or a value outside the shape its
//! registry entry accepts refuses the start with exit 78 (plan C7), one line per problem.
//! It runs first so an operator's typo is the first thing they read, rather than a missing
//! behaviour they deduce three screens down. Phase 3 likewise checks the store root's
//! filesystem before opening it, and a store-open refusal that needs operator action
//! exits 65 (75 for a locked root).

pub(crate) mod config;
pub(crate) mod daemons;
pub(crate) mod hooks;
pub(crate) mod serve;
pub(crate) mod state;

/// The startup phases in the order [`run`] executes them (SEC-8). The
/// module docs above describe each one; the test below pins this list
/// against `run`'s actual call order, so reordering a phase means
/// editing this list and the docs in the same change — never silently.
pub(crate) const PHASES: [&str; 7] = ["strict-config", "hooks", "signals", "config", "state", "daemons", "serve"];

/// Run every startup phase, in order, until the server stops serving.
///
/// # Errors
///
/// Any phase that refuses to start the server: a malformed address, a malformed
/// operator setting, a segment store that cannot be opened, or a listener that
/// cannot be bound. A background task failing does not fail startup.
/// Refuse startup for a configuration problem (PAC-5b): print every problem,
/// then exit with the plan C7 status for invalid configuration (78). Startup
/// phases that validate operator settings route their errors here, so a bad
/// address, cap, TLS pair, keys file or daemon setting is the same class of
/// refusal as a bad variable name or value.
fn exit_config(error: &anyhow::Error) -> ! {
	eprintln!("weft-server: invalid configuration: {error:#}");
	std::process::exit(weft_server::settings::CONFIG_EXIT_CODE);
}

pub(crate) async fn run() -> anyhow::Result<()> {
	// First, before any phase reads a setting or opens a store: strict
	// configuration (PAC-5b). What PAC-5a's registry only warned about — an
	// unknown `WEFT_` name, a test-harness channel set on a real server, a
	// value outside the shape its registry entry accepts — now refuses the
	// start, exit 78, because a silently ignored setting is the most
	// expensive kind of configuration bug.
	let strict_errors = weft_server::settings::strict_errors_from_env();
	if !strict_errors.is_empty() {
		for error in &strict_errors {
			eprintln!("weft-server: {error}");
		}
		eprintln!("weft-server: refusing to start: {} configuration problem(s) above (exit {})", strict_errors.len(), weft_server::settings::CONFIG_EXIT_CODE);
		std::process::exit(weft_server::settings::CONFIG_EXIT_CODE);
	}

	println!("startup phases, in order: {}", PHASES.join(" -> "));

	// The OTLP provider (when configured) is held for the process lifetime: dropping it
	// early would tear down the batch span processor and stop export. It is shut down on
	// a clean exit to flush any buffered spans.
	let otel_provider = hooks::init_tracing();
	// Immediately after tracing, so the panic hook reports through the subscriber it chains.
	hooks::init_panic_hook();

	// Handlers first (C9): SIGINT/SIGTERM are registered before any phase that
	// can block, so a signal before serving is recorded rather than killing
	// the process mid-startup, and serve::run observes it on entry.
	let mut shutdown = weft_server::ShutdownSignals::install()?;

	let addr = config::bind_addr().unwrap_or_else(|error| exit_config(&error));
	let interpolate = config::interpolate_config().unwrap_or_else(|error| exit_config(&error));
	let limits = config::serve_limits().unwrap_or_else(|error| exit_config(&error));
	// D-S14: the ingest wiring's limits, read once like every other
	// setting, so the registry the state builds carries the operator's
	// body cap and in-flight budget rather than the defaults.
	let ingest = config::ingest_config().unwrap_or_else(|error| exit_config(&error));
	// ROB-10: Admission I's limits, read once alongside the ingest
	// wiring's, so the gate the registry builds carries the operator's
	// query-class cap rather than the default.
	let admission = config::admission_config().unwrap_or_else(|error| exit_config(&error));
	// SEC-5: resolve the transport (TLS when configured) and run the C8
	// plaintext gate against the address about to be bound, before the
	// store opens — a refused exposure starts nothing at all.
	let tls = config::transport(addr).unwrap_or_else(|error| exit_config(&error));
	// SEC-6: load the keys file (when one is mounted) before the store
	// opens — a keys file that cannot be loaded stops startup, and the
	// server never comes up unauthenticated by accident.
	let auth = config::auth().unwrap_or_else(|error| exit_config(&error));
	let state = match state::build().await {
		Ok(state) => state.with_interpolate_config(interpolate).with_auth(auth).with_ingest_config(ingest).with_admission_config(admission),
		Err(error) => match weft_server::store_open::exit_code_for_store_error(&error) {
			// A store that needs operator action (65) or a locked root (75):
			// the plan C7 class, not a generic failure.
			Some(code) => {
				eprintln!("weft-server: the segment store cannot be opened: {error:#}");
				std::process::exit(code);
			}
			None => return Err(error),
		},
	};

	// ROB-3: the Daemons registry owns every background task this process
	// runs — supervised, and stopped on its shared shutdown watch (the
	// watch D-S14's amendment shares with this registry) once serving ends.
	let daemons = weft_server::Daemons::new().with_audit_log(state.audit_log().cloned());

	daemons::spawn_reconcile_if_configured(&state, &daemons)?;
	daemons::spawn_backup_if_configured(&state, &daemons)?;

	let listener = serve::bind(addr, tls.is_some()).await?;
	let state = state.with_host_guard(config::host_guard_for(serve::local_addr(&listener)?));

	// After the bind and in the background, so serving never waits for it.
	daemons::spawn_gpu_calibration_if_enabled(&daemons);

	serve::run(listener, state.clone(), otel_provider, limits, tls, &mut shutdown).await?;
	// Serving is over: raise the registry's shutdown watch so no daemon
	// outlives the server it maintains, and none restarts from here.
	daemons.shutdown();
	// C9's drain order: serve → SEC-9 TaskTracker → store.shutdown().
	// The audit tracker drains next, so an audited handler still in
	// flight when serving ended writes its outcome record before the
	// store closes under it.
	if let Some(log) = state.audit_log() {
		log.close_and_drain().await;
	}
	// Serving stopped cleanly (including a signal before serving, which exits 0
	// without accepting a connection): shut the store down — the closing
	// reaper pass, then the clean-shutdown commit (D-S14's
	// `SegmentStore::shutdown`, crash-consistency §5.5) — so the next
	// open's `/ready` can report `previous_clean_shutdown: true` (D-S12). A
	// serve failure is not a clean shutdown, and leaves the flag at the `0`
	// the open wrote.
	if let Some(store) = state.store() {
		store.shutdown().await?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::PHASES;

	/// SEC-8: the phase list is enforced. `run`'s body must call each
	/// phase's entry point in exactly [`PHASES`] order — the strict
	/// check before tracing, tracing before the signal handlers, the
	/// handlers before config, config (with the exposure matrix and the
	/// keys file) before the store opens, daemons before the bind, and
	/// serving last. The markers are the calls themselves, read from
	/// this file's own source after `run`'s signature, so a reorder
	/// that does not also update [`PHASES`] and the module docs fails
	/// here instead of shipping as a comment that lied.
	#[test]
	fn the_phase_list_matches_runs_call_order() {
		assert_eq!(PHASES, ["strict-config", "hooks", "signals", "config", "state", "daemons", "serve"], "the frozen phase order; changing it is a design change, not a refactor");
		let source = include_str!("mod.rs");
		let run_body = source.split("pub(crate) async fn run()").nth(1).expect("run exists in this file");
		let markers = ["settings::strict_errors_from_env", "hooks::init_tracing", "ShutdownSignals::install", "config::bind_addr", "state::build", "daemons::spawn_reconcile_if_configured", "serve::run("];
		assert_eq!(markers.len(), PHASES.len(), "one marker per phase");
		let mut previous: Option<usize> = None;
		for (phase, marker) in PHASES.iter().zip(markers) {
			let position = run_body.find(marker).unwrap_or_else(|| panic!("phase {phase}: marker {marker:?} not found in run's body"));
			if let Some(previous) = previous {
				assert!(position > previous, "phase {phase} (marker {marker:?}) runs out of order in run's body");
			}
			previous = Some(position);
		}
	}
}
