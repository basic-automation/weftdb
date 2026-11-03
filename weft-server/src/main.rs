//! `weft-server` binary entry point: the process runtime and one call into
//! [`startup`].
//!
//! The runtime is built explicitly (ROB-11, [`weft_server::runtime`]):
//! `main` reads its shape, builds it with tokio's runtime builder, runs
//! the dispatch on it, and shuts it down with plan C9's
//! `shutdown_timeout(5 s)` — the last step of the drain order, after
//! serving, the task drain and the store's shutdown have all finished
//! inside [`startup`].
//!
//! Every startup phase lives in [`startup`], in the order it runs, so this file
//! carries no behaviour of its own — the router under test is exactly the router
//! served, and a later wave can change one startup phase without touching this
//! file.

mod startup;

// The process allocator is this binary's choice, not a side effect of what a library it links
// happens to depend on (PAC-3). turso used to carry it: its `mimalloc` feature installs a
// `#[global_allocator]` in turso's own crate root, so any program linking `weftdb` got mimalloc
// whether it wanted it or not. `weftdb` no longer asks for it, and an executable that wants it —
// every one of ours, on every machine we benchmark and ship — says so here.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
	// The runtime's shape is read before anything async can run — the
	// runtime must exist before even the strict-configuration phase
	// inside `startup` can execute — so a malformed `WEFT_WORKER_THREADS`
	// is refused here, with the plan C7 status for invalid configuration
	// (78) that the strict phase would have used for the same value.
	let config = weft_server::RuntimeConfig::from_env().unwrap_or_else(|error| {
		eprintln!("weft-server: invalid configuration: {error}");
		std::process::exit(weft_server::settings::CONFIG_EXIT_CODE);
	});
	weft_server::runtime::run(config, dispatch())?
}

async fn dispatch() -> anyhow::Result<()> {
	// A word on the command line is a command, and there are exactly three besides serving: a
	// misspelled `import-rows-mode` must not quietly come up as a server instead of
	// migrating the tree the operator was pointing at, `keygen` must not either, and above
	// all a misspelled `restore` must not stand up a live server on the machine that was
	// meant to be putting a store back. Any other word goes to `import_rows_mode`, which
	// refuses it rather than serving.
	let mut args = std::env::args();
	args.next();
	match args.next().as_deref() {
		None => startup::run().await,
		Some(weft_server::keygen::SUBCOMMAND) => weft_server::keygen::run(std::env::args().skip(1)),
		Some(weft_server::restore::SUBCOMMAND) => weft_server::restore::run(std::env::args().skip(1)).await,
		Some(_command) => weft_server::import_rows_mode::run(std::env::args().skip(1)).await,
	}
}
