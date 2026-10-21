//! `weft-server` binary entry point: the process runtime and one call into
//! [`startup`].
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	startup::run().await
}
