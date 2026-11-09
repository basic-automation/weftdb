#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! `weft-server` binary entry point: the process runtime and one call into
//! the operator CLI ([`weft_server::cli`], PAC-7).
//!
//! The CLI parses the command line into a command before anything runs —
//! serve by default, or one of the operator subcommands — so this file
//! carries no behaviour of its own beyond the dispatch: the router under
//! test is exactly the router served, and a later wave can change one
//! startup phase or one subcommand without touching this file.
//!
//! The runtime is built explicitly (ROB-11, [`weft_server::runtime`]):
//! `main` reads its shape, builds it with tokio's runtime builder, runs
//! the dispatch on it, and shuts it down with plan C9's
//! `shutdown_timeout(5 s)` — the last step of the drain order, after
//! serving, the task drain and the store's shutdown have all finished
//! inside [`startup`].
//!
//! Every startup phase lives in [`startup`], in the order it runs.
//!
//! Exit statuses are plan C7's table ([`weft_server::cli::EXIT_CODES`]):
//! a usage error the CLI or a subcommand's parser finds exits 2 here
//! (or inside the command, for the ones that exit themselves), a
//! configuration refusal exits 78, a store that needs operator action
//! 65, a locked root 75, a poison exit 70 — and anything no phase
//! classified falls out of this function's `anyhow::Result` return as
//! the runtime's 1, the unexpected error.

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
	let argv: Vec<String> = std::env::args().skip(1).collect();
	match weft_server::cli::parse(&argv) {
		Ok(weft_server::cli::Command::Serve) => startup::run().await,
		Ok(weft_server::cli::Command::Help) => {
			println!("{}", weft_server::cli::USAGE);
			Ok(())
		}
		Ok(weft_server::cli::Command::Version) => {
			println!("{} {}", weft_server::SERVICE, weft_server::VERSION);
			Ok(())
		}
		Ok(weft_server::cli::Command::Keygen(arguments)) => weft_server::keygen::run(arguments),
		Ok(weft_server::cli::Command::Restore(arguments)) => weft_server::restore::run(arguments.into_iter()).await,
		Ok(weft_server::cli::Command::ImportRowsMode(arguments)) => weft_server::import_rows_mode::run(arguments).await,
		Ok(weft_server::cli::Command::Openapi(arguments)) => weft_server::openapi::run(arguments),
		Err(error) => {
			eprintln!("weft-server: {error}");
			std::process::exit(weft_server::settings::USAGE_EXIT_CODE);
		}
	}
}
