//! Peak memory of opening one MVCC file: `cargo run --release --example open_rss -- <file.db>`.
use weftdb::{database::traits::DatabaseStructure, Database};

fn rss_kb() -> u64 {
	std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let path = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: open_rss <file.db>"))?;
	let before = rss_kb();
	let db = Database::get_turso_database(&path).await?;
	let after_open = rss_kb();
	let conn = db.connect()?;
	let sql = std::env::args().nth(2).unwrap_or_else(|| "SELECT COUNT(*) FROM sqlite_schema".into());
	for pass in 1..=3 {
		let mut rows = conn.query(&sql, ()).await?;
		let first = rows.next().await?.map(|r| format!("{:?}", r.get_value(0)));
		drop(rows);
		println!("pass {pass}: {first:?} rss={} kB", rss_kb());
	}
	drop(conn);
	let conn = db.connect()?;
	let mut rows = conn.query(&sql, ()).await?;
	let _ = rows.next().await?;
	drop((rows, conn));
	println!("rss before={before} kB, after open={after_open} kB, after new connection={} kB", rss_kb());
	Ok(())
}
