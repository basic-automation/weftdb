//! Does memory keep climbing when many MVCC stores each take small commits? Opens `STORES`
//! databases through WeftDB's own open path, commits small rows to them round-robin, and
//! prints RSS as the logs grow:
//! `cargo run --release --example many_store_rss -- <empty dir> [stores] [rounds]`.
//! Models a long-running app such as a fan daemon: hundreds of stores, none of whose
//! `-log` reaches Turso's checkpoint threshold on its own.
use weftdb::{database::traits::DatabaseStructure, Database};

fn rss_kb() -> u64 {
	std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or(0)
}

fn log_bytes(dir: &std::path::Path) -> u64 {
	std::fs::read_dir(dir).into_iter().flatten().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "db-log")).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let mut args = std::env::args().skip(1);
	let root = std::path::PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("usage: many_store_rss <empty dir> [stores] [rounds]"))?);
	let stores: usize = args.next().map_or(Ok(559), |s| s.parse())?;
	let rounds: usize = args.next().map_or(Ok(400), |s| s.parse())?;
	std::fs::create_dir_all(&root)?;
	let mut dbs = Vec::with_capacity(stores);
	for i in 0..stores {
		let p = root.join(format!("s{i}.db"));
		let (db, _) = Database::get_or_create_turso_database(&p.to_string_lossy()).await?;
		db.connect()?.execute("CREATE TABLE IF NOT EXISTS m (t INTEGER PRIMARY KEY, v BLOB)", ()).await?;
		dbs.push(db);
	}
	println!("round,rss_kb,log_mb");
	let blob = vec![7u8; 200];
	for r in 0..rounds {
		for db in &dbs {
			let c = db.connect()?;
			for k in 0..10 {
				c.execute("INSERT INTO m (t, v) VALUES (?1, ?2)", turso::params![(r * 10 + k) as i64, blob.clone()]).await?;
			}
		}
		if r % 20 == 0 || r + 1 == rounds {
			println!("{r},{},{:.1}", rss_kb(), log_bytes(&root) as f64 / 1e6);
		}
	}
	Ok(())
}
