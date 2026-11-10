//! Prune a store's unbounded metadata rows: `WEFT_DATA_DIR=<dir> cargo run --release --example prune_metadata -- <database>`.
//!
//! Opening a database prunes them too; this reports the time and count.
use weftdb::{database::traits::DatabaseStructure, Database};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let name = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: prune_metadata <database>"))?;
	let started = std::time::Instant::now();
	let database = Database::existing(&name).await?;
	println!("opened (with prune) in {:?}", started.elapsed());
	println!("second prune removed {} rows", database.prune_metadata().await?);
	Ok(())
}
