//! Does dropping idle handles return the per-file arena? Opens every `*.db` under a store
//! directory, then closes them all and reports RSS:
//! `cargo run --release --example idle_close_rss -- <store dir>`.
use weftdb::{database::traits::DatabaseStructure, Database};

fn rss_kb() -> u64 {
	std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or(0)
}

fn dbs(dir: &std::path::Path, out: &mut Vec<String>) {
	for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
		let p = e.path();
		if p.is_dir() {
			dbs(&p, out);
		} else if p.extension().is_some_and(|x| x == "db") && !p.ends_with("metadata.db") {
			out.push(p.to_string_lossy().into_owned());
		}
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let root = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: idle_close_rss <store dir>"))?;
	let mut paths = Vec::new();
	dbs(std::path::Path::new(&root), &mut paths);
	let before = rss_kb();
	for p in &paths {
		let db = Database::get_turso_database(p).await?;
		let conn = db.connect()?;
		let mut rows = conn.query("SELECT COUNT(*) FROM sqlite_schema", ()).await?;
		let _ = rows.next().await?;
	}
	let open = rss_kb();
	let closed = weftdb::close_idle_databases(std::time::Duration::ZERO).await;
	let after = rss_kb();
	println!("files={} rss before={before} kB, all open={open} kB, closed {closed} -> {after} kB (cache now {})", paths.len(), weftdb::open_database_count().await);
	Ok(())
}
