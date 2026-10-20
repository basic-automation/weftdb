//! `weft-server` binary entry point: the process runtime and one call into
//! [`startup`].
//!
//! Every startup phase lives in [`startup`], in the order it runs, so this file
//! carries no behaviour of its own — the router under test is exactly the router
//! served, and a later wave can change one startup phase without touching this
//! file.

mod startup;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	startup::run().await
}
