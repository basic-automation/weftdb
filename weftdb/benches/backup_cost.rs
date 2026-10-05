//! Benchmark the **control-plane backup tick** (`SegmentStore::backup_control_plane_with_verify`)
//! — the four `VACUUM INTO` snapshots plus their verification that the backup daemon
//! runs every `WEFT_BACKUP_INTERVAL_SECS` (roadmap Phase 7.4).
//!
//! ## What this measures, and why it exists
//!
//! The roadmap carries a runtime observation that a snapshot tick on a **five-row**
//! control plane landed 7–22 s apart at a 2–3 s configured interval — i.e. the interval
//! was a floor, not a cadence — with the caveat that it was observed on a box running
//! cargo builds at the same time, so the absolute numbers were soft. This bench is the
//! proper measurement on a quiet box: it seals a store with `aspects` aspects (a few
//! segments each, so every control-plane DB carries real rows) and times one whole
//! backup tick into a fresh directory, under both verify modes.
//!
//! The aspect *count* is the swept axis: the control plane's row count grows with it
//! (one `segment_index` row per segment, one `aspect_metadata` / `aspect_catalog` row per
//! aspect), so the sweep shows whether the tick cost is a per-database floor (flat across
//! the axis) or scales with the rows the vacuum has to copy. Every iteration vacuums into
//! its own fresh temp dir — `VACUUM INTO` refuses an existing file — so the setup cost of
//! creating that dir is excluded from the timing by `iter_batched`.
//!
//! ## The filesystem is the variable — set `WEFT_BENCH_BACKUP_DIR`
//!
//! **A default run measures the system temp directory, which on a typical Linux box is
//! `tmpfs` (RAM) and therefore reports the vacuum's CPU cost with no durability cost at
//! all.** A control plane that lives on a real disk pays a very different price, and a
//! backup-cadence question is a *durability* question — so the number that matters comes
//! from pointing this bench at the volume the store actually lives on:
//!
//! ```text
//! WEFT_BENCH_BACKUP_DIR=/srv/weftdb/benchtmp cargo bench -p database --bench backup_cost
//! ```
//!
//! Both the populated store and the backup destinations are created under that base, so the
//! whole read+write path is on the target filesystem. The label printed at startup says
//! which base was used, so no result is ambiguous about what it measured.

use std::{hint::black_box, path::PathBuf};

use bigdecimal::BigDecimal;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::{SegmentStore, VerifyMode};

/// The base directory temp stores/backups are created under: `WEFT_BENCH_BACKUP_DIR` when
/// set, else the system temp dir (often `tmpfs` — see the module docs).
fn base_dir() -> Option<PathBuf> {
	std::env::var_os("WEFT_BENCH_BACKUP_DIR").map(PathBuf::from)
}

/// A fresh temp dir under [`base_dir`], creating the base if it does not exist.
fn temp_dir() -> TempDir {
	base_dir().map_or_else(
		|| TempDir::new().expect("temp dir"),
		|base| {
			std::fs::create_dir_all(&base).expect("creates bench base dir");
			TempDir::new_in(&base).expect("temp dir in bench base")
		},
	)
}

/// Segments sealed per aspect, so the segment index holds `aspects * SEGMENTS_PER_ASPECT`
/// rows.
const SEGMENTS_PER_ASPECT: i64 = 4;

/// Rows per sealed segment. Small — this bench times the control plane, not the seal.
const ROWS_PER_SEGMENT: i64 = 64;

/// Open a store under `dir` and seal `aspects` aspects with [`SEGMENTS_PER_ASPECT`]
/// segments each, so all four control-plane databases carry rows proportional to `aspects`.
async fn populated_store(dir: &TempDir, aspects: usize) -> SegmentStore {
	let store = SegmentStore::open(dir.path()).await.expect("opens store");
	let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
	for a in 0..aspects {
		let name = format!("aspect_{a}");
		store.declare(&name, &schema).await.expect("declares aspect");
		for seg in 0..SEGMENTS_PER_ASPECT {
			let ts: Vec<i64> = (0..ROWS_PER_SEGMENT).map(|i| seg * ROWS_PER_SEGMENT + i).collect();
			let vs: Vec<BigDecimal> = (0..ROWS_PER_SEGMENT).map(|i| BigDecimal::from((seg * ROWS_PER_SEGMENT + i) % 97)).collect();
			store.seal_declared(&name, &ts, &vs).await.expect("seals");
		}
	}
	store
}

/// Sweep the aspect count under each verify mode. A flat curve means the tick cost is a
/// per-database fixed floor (four vacuums + four reopens); a rising one means it tracks
/// the rows copied.
fn bench_backup_tick(c: &mut Criterion) {
	let rt = Runtime::new().expect("tokio runtime");
	eprintln!("backup_cost base dir: {} (set WEFT_BENCH_BACKUP_DIR to measure a real volume)", base_dir().map_or_else(|| std::env::temp_dir().display().to_string(), |b| b.display().to_string()));

	let mut group = c.benchmark_group("backup/control_plane_tick");
	group.sample_size(10);
	for (label, mode) in [("snapshot_only", VerifyMode::SnapshotOnly), ("source_match", VerifyMode::SourceMatch)] {
		for aspects in [1_usize, 16, 128] {
			// Populate once per arm — the sweep measures the backup, not the seal.
			let dir = temp_dir();
			let store = rt.block_on(populated_store(&dir, aspects));
			group.bench_with_input(BenchmarkId::new(label, aspects), &aspects, |b, _| {
				b.iter_batched(
					temp_dir,
					|dest| {
						let backup = rt.block_on(store.backup_control_plane_with_verify(dest.path().join("tick"), mode)).expect("backs up");
						black_box(backup.total_rows());
						// Keep the dest dir alive until the tick has been timed.
						drop(dest);
					},
					BatchSize::PerIteration,
				);
			});
			drop(store);
			drop(dir);
		}
	}
	group.finish();
}

criterion_group!(benches, bench_backup_tick);
criterion_main!(benches);
