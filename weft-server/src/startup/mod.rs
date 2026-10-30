//! The startup phases of the `weft-server` binary, in the order they run.
//!
//! The entry point ([`main`](crate)) only installs the runtime; what happens
//! after that is this list. Each phase is one module, so a later wave edits a
//! startup behaviour by editing that module rather than by growing a single file.
//!
//! The order matters:
//!
//! 1. [`hooks`] — the tracing subscriber and the OTLP exporter when configured, then the
//!    panic hook that ends the process when a control-plane write panics.
//!    Installed first so every later phase is observable.
//! 2. [`config`] — what the operator asked for: bind address, interpolation cap,
//!    and (once the address is known) the request guard.
//! 3. [`state`] — the router state, which means opening the segment store when
//!    one is configured.
//! 4. [`daemons`] — the background reconcile/backup/GPU-calibration tasks.
//! 5. [`serve`] — bind, print the banner, serve until shutdown.
//!
//! The shutdown signal handlers are installed before phase 2 (right after
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
//! ## Transport (SEC-5)
//!
//! With `WEFT_TLS_CERT_FILE` and `WEFT_TLS_KEY_FILE` set, the listener speaks
//! TLS (rustls, ring provider) on the same serve path; the banner says
//! `https`. Without them the listener is plaintext, and a non-loopback bind
//! must pass the C8 gate before anything is served: a mounted keys file
//! (`WEFT_KEYS_FILE`) plus `WEFT_ALLOW_PLAINTEXT=1`, or `WEFT_AUTH=disabled`
//! behind a gateway — otherwise startup refuses. Loopback plaintext is
//! ungated. See [`weft_server::tls`].
//!
//! ## Authentication (SEC-6)
//!
//! With `WEFT_KEYS_FILE` naming a keys file (or `WEFT_AUTH=keys`, which
//! requires one), every request is authenticated with
//! `Authorization: Bearer <key>` against the file's key hashes, and the
//! key's role is checked against the scope the sealed registry assigns
//! the route ([`weft_server::auth`]). The probes stay open to the
//! anonymous principal; everything else needs a key. `WEFT_AUTH=disabled`
//! turns authentication explicitly off, and with no keys file mounted
//! the server runs unauthenticated, as it did before SEC-6.
//! `weft-server keygen` mints a key and records its hash in the file.
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
//! Before the first phase, [`weft_server::settings::report`] checks the environment against the
//! registry of every variable the workspace reads: a `WEFT_` name no crate reads, or a test
//! harness's own channel set on a real server, prints one line there. It runs first so an
//! operator's typo is the first thing they read, rather than a missing behaviour they
//! deduce three screens down.

pub(crate) mod config;
pub(crate) mod daemons;
pub(crate) mod hooks;
pub(crate) mod serve;
pub(crate) mod state;

/// Run every startup phase, in order, until the server stops serving.
///
/// # Errors
///
/// Any phase that refuses to start the server: a malformed address, a malformed
/// operator setting, a segment store that cannot be opened, or a listener that
/// cannot be bound. A background task failing does not fail startup.
pub(crate) async fn run() -> anyhow::Result<()> {
	// First, before any phase reads a setting or opens a store: what the environment got
	// wrong, answered from the registry in [`crate::settings`].
	weft_server::settings::report();

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

	let addr = config::bind_addr()?;
	let interpolate = config::interpolate_config()?;
	let limits = config::serve_limits()?;
	// SEC-5: resolve the transport (TLS when configured) and run the C8
	// plaintext gate against the address about to be bound, before the
	// store opens — a refused exposure starts nothing at all.
	let tls = config::transport(addr)?;
	// SEC-6: load the keys file (when one is mounted) before the store
	// opens — a keys file that cannot be loaded stops startup, and the
	// server never comes up unauthenticated by accident.
	let auth = config::auth()?;
	let state = state::build().await?.with_interpolate_config(interpolate).with_auth(auth);

	daemons::spawn_reconcile_if_configured(&state)?;
	daemons::spawn_backup_if_configured(&state)?;

	let listener = serve::bind(addr, tls.is_some()).await?;
	let state = state.with_host_guard(config::host_guard_for(serve::local_addr(&listener)?));

	// After the bind and in the background, so serving never waits for it.
	daemons::spawn_gpu_calibration_if_enabled();

	serve::run(listener, state.clone(), otel_provider, limits, tls, &mut shutdown).await?;
	// Serving stopped cleanly (including a signal before serving, which exits 0
	// without accepting a connection): record the clean shutdown, so the next
	// open's `/ready` can report `previous_clean_shutdown: true` (D-S12). A
	// serve failure is not a clean shutdown, and leaves the flag at the `0`
	// the open wrote.
	if let Some(store) = state.store() {
		store.mark_clean_shutdown().await?;
	}
	Ok(())
}
