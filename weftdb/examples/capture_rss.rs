//! Memory per committed row, raw Turso vs `capture_measurement`:
//! `cargo run --release --example capture_rss -- <empty dir> <raw|raw-noidx|conns|weft> [rows]`.
//!
//! `raw` commits one row per `BEGIN CONCURRENT` on a fresh connection to a WeftDB-configured
//! MVCC file (as `begin_concurrent` does); `weft` captures into one seconds aspect.
#![allow(unused_imports)]
use std::str::FromStr;

use bigdecimal::BigDecimal;
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs},
	Database, DatasetId, InputMeasurement,
};

/// Keep every opened database alive for the whole run, as WeftDB's handle cache does.
fn held_dbs(db: turso::Database) {
	static DBS: std::sync::Mutex<Vec<turso::Database>> = std::sync::Mutex::new(Vec::new());
	DBS.lock().expect("not poisoned").push(db);
}

fn anon_mb() -> f64 {
	let s = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
	s.lines().find(|l| l.starts_with("Anonymous:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
	// NOTHP=1: opt out of transparent huge pages, as weft-server does, to A/B what THP=always adds.
	if std::env::var("NOTHP").is_ok() {
		weftdb::opt_out_of_transparent_huge_pages()?;
	}
	let mut args = std::env::args().skip(1);
	let dir = args.next().ok_or_else(|| anyhow::anyhow!("usage: capture_rss <dir> <raw|raw-noidx|conns|weft> [rows]"))?;
	let mode = args.next().unwrap_or_else(|| "raw".into());
	let rows: i64 = args.next().map_or(Ok(200_000), |r| r.parse())?;
	std::fs::create_dir_all(&dir)?;
	std::env::set_var("WEFT_DATA_DIR", &dir);
	let start = chrono::Utc::now() - chrono::Duration::seconds(rows);
	let base = anon_mb();
	let step = (rows / 10).max(1);
	println!("mode={mode} rows={rows} anon_start={base:.0} MB");

	match mode.as_str() {
		"raw" | "raw-noidx" => {
			let path = format!("{dir}/raw.db");
			let mut db = turso::Builder::new_local(&path).build().await?;
			let conn = db.connect()?;
			if std::env::var("NOMVCC").is_err() { conn.execute("PRAGMA journal_mode=experimental_mvcc", ()).await.ok(); }
			let threshold = std::env::var("WEFT_MVCC_CHECKPOINT_BYTES").unwrap_or_else(|_| "65536".into());
			conn.execute(&format!("PRAGMA mvcc_checkpoint_threshold={threshold}"), ()).await.ok();
			conn.execute("CREATE TABLE m (ts INTEGER NOT NULL, dataset TEXT, value TEXT)", ()).await?;
			if mode == "raw" {
				conn.execute("CREATE UNIQUE INDEX m_ts ON m (ts)", ()).await?;
			}
			drop(conn);
			for i in 0..rows {
				let per_tx: i64 = std::env::var("PER_TX").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
				if std::env::var("CONNECT_ONLY").is_ok() {
					let conn = db.connect()?;
					drop(conn);
				} else if std::env::var("REUSE_CONN").is_ok() {
					thread_local! { static C: std::cell::RefCell<Option<turso::Connection>> = const { std::cell::RefCell::new(None) }; }
					let conn = C.with(|c| c.borrow_mut().get_or_insert_with(|| db.connect().expect("connect")).clone());
					conn.execute("BEGIN CONCURRENT", ()).await?;
					conn.execute("INSERT INTO m (ts, dataset, value) VALUES (?, ?, ?)", turso::params![(start + chrono::Duration::seconds(i)).timestamp_millis(), "d", "1.0"]).await?;
					conn.execute("COMMIT", ()).await?;
				} else if i % per_tx == 0 {
					let conn = db.connect()?;
					conn.execute(if std::env::var("NOMVCC").is_ok() { "BEGIN" } else { "BEGIN CONCURRENT" }, ()).await?;
					for j in i..(i + per_tx).min(rows) {
						conn.execute("INSERT INTO m (ts, dataset, value) VALUES (?, ?, ?)", turso::params![(start + chrono::Duration::seconds(j)).timestamp_millis(), "d", format!("{:.3}", 40.0 + (j % 100) as f64 / 10.0)]).await?;
					}
					conn.execute("COMMIT", ()).await?;
					drop(conn);
				}
				if (i + 1) % step == 0 {
					if std::env::var("IDLE_CKPT").is_ok() {
						let c = db.connect()?;
						let r = match c.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await { Ok(mut rows) => rows.next().await.map(|r| r.map(|r| format!("{:?}", r.get_value(0)))), Err(e) => Err(e) };
						if i + 1 == step { println!("idle checkpoint: {r:?}"); }
					}
					if std::env::var("REOPEN").is_ok() {
						drop(db);
						println!("  after drop anon={:.0} MB", anon_mb());
						db = turso::Builder::new_local(&path).build().await?;
						let c = db.connect()?;
						c.execute("PRAGMA journal_mode=experimental_mvcc", ()).await.ok();
						c.execute("PRAGMA mvcc_checkpoint_threshold=65536", ()).await.ok();
					}
					let a = anon_mb();
					println!("{},{a:.0},{:.0} B/row", i + 1, (a - base) * 1024.0 * 1024.0 / (i + 1) as f64);
				}
			}
		}
		// `conns`: open `rows` databases, run one transaction on each, and keep (KEEP=1) or drop
		// its connection, optionally after `PRAGMA cache_size=CACHE` — what a pooled idle
		// connection costs per file.
		"conns" => {
			let keep = std::env::var("KEEP").is_ok();
			let cache = std::env::var("CACHE").ok();
			let mut held = Vec::new();
			for i in 0..rows {
				let db = turso::Builder::new_local(&format!("{dir}/c{i}.db")).build().await?;
				let conn = db.connect()?;
				conn.execute("PRAGMA journal_mode=experimental_mvcc", ()).await.ok();
				conn.execute("CREATE TABLE IF NOT EXISTS m (ts INTEGER)", ()).await?;
				let conn = db.connect()?;
				if let Some(c) = &cache { conn.execute(&format!("PRAGMA cache_size={c}"), ()).await?; }
				match std::env::var("WORK").as_deref() {
					Ok("none") => {}
					Ok("read") => {
						let mut r = conn.query("SELECT count(*) FROM m", ()).await?;
						while r.next().await?.is_some() {}
					}
					_ => {
						conn.execute("BEGIN CONCURRENT", ()).await?;
						conn.execute("INSERT INTO m VALUES (?)", turso::params![i]).await?;
						conn.execute("COMMIT", ()).await?;
					}
				}
				if keep { held.push(conn); }
				held_dbs(db);
				if (i + 1) % step == 0 {
					let a = anon_mb();
					println!("{},{a:.0},{:.0} KB/db", i + 1, (a - base) * 1024.0 / (i + 1) as f64);
				}
			}
			drop(held);
		}
		"weft" => {
			let db = Database::new("capture_rss").await?;
			let subject = db.observe_subject("s").await?;
			let aspect = db.track_aspect(&subject.id(), "a", &splimes::Resolution::Seconds, None).await?.id();
			let dataset = DatasetId::new();
			for i in 0..rows {
				let v = BigDecimal::from_str(&format!("{:.3}", 40.0 + (i % 100) as f64 / 10.0))?;
				Inputs::capture_measurement(&db, &aspect, &dataset, &InputMeasurement::new(start + chrono::Duration::seconds(i), v)).await?;
				if (i + 1) % step == 0 {
					let a = anon_mb();
					println!("{},{a:.0},{:.0} B/row", i + 1, (a - base) * 1024.0 * 1024.0 / (i + 1) as f64);
				}
			}
		}
		other => anyhow::bail!("unknown mode {other}"),
	}
	println!("end anon={:.0} MB", anon_mb());
	Ok(())
}
