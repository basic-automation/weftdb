//! Prune a store's unbounded metadata rows: `WEFT_DATA_DIR=<dir> cargo run --release --example prune_metadata -- <database>`.
//!
//! Opening a database prunes them too; this reports the time and count.
use weftdb::{database::traits::DatabaseStructure, Database};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let name = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: prune_metadata <database>"))?;
	let started = std::time::Instant::now();
	let database = Database::existing(&name).await?;
	println!("opened (with prune) in {:?}, rss {} kB", started.elapsed(), rss_kb());
	println!("second prune removed {} rows, rss {} kB", database.prune_metadata().await?, rss_kb());
	Ok(())
}

fn rss_kb() -> u64 {
	std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or(0)
}
