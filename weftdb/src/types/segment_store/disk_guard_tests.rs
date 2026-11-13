//! ROB-17: a seal that would leave the store's filesystem below the disk
//! guard's reserve is refused before any frame is written. The reserve rides
//! [`SegmentStoreOptions`], so a test holds an impossible reserve (more than
//! any disk) and watches the refusal on a real filesystem, with no fake.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{SegmentStore, SegmentStoreOptions};
use crate::{IngestBatch, StoreError};

/// The aspect every test here writes.
const ASPECT: &str = "price";

/// A reserve no filesystem can satisfy.
const IMPOSSIBLE: u64 = u64::MAX / 2;

fn values(count: i64) -> (Vec<i64>, Vec<BigDecimal>) {
	(0..count).map(|t| (t, BigDecimal::from_str(&t.to_string()).expect("parses"))).unzip()
}

async fn store_at(root: &Path, reserve: u64) -> SegmentStore {
	let store = SegmentStore::open_with_options(root, SegmentStoreOptions::default().with_disk_reserve_bytes(reserve)).await.expect("opens");
	store.declare(ASPECT, &AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)).await.expect("declares");
	store
}

/// The `.weftseg` frames under the store's `segments/` directory.
fn frames(root: &Path) -> usize {
	std::fs::read_dir(root.join("segments")).map_or(0, |dir| dir.filter_map(Result::ok).filter(|entry| entry.path().extension().is_some_and(|ext| ext == "weftseg")).count())
}

#[test]
fn the_library_default_leaves_the_guard_off() {
	assert_eq!(SegmentStoreOptions::default().disk_reserve_bytes, 0, "a library open refuses nothing it did not refuse before ROB-17");
	assert_eq!(SegmentStoreOptions::default().with_disk_reserve_bytes(7).disk_reserve_bytes, 7);
}

#[tokio::test]
async fn an_ingest_below_the_reserve_writes_nothing() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), IMPOSSIBLE).await;
	let (ts, vs) = values(1_000);
	let err = store.ingest(ASPECT, &IngestBatch::dense("k1", ts, vs)).await.expect_err("the guard refuses");
	let refusal = err.downcast_ref::<StoreError>().expect("a typed refusal through the ingest's context");
	let StoreError::InsufficientStorage { reserve_bytes, need_bytes, available_bytes } = refusal else {
		panic!("expected InsufficientStorage, got {refusal:?}");
	};
	assert_eq!(*reserve_bytes, IMPOSSIBLE);
	assert!(*need_bytes > 0 && *available_bytes < IMPOSSIBLE);
	assert_eq!(frames(dir.path()), 0, "no frame was written");
	assert_eq!(store.segment_count(ASPECT).await.unwrap(), 0, "nothing was published");
	assert!(store.headroom().is_some_and(|headroom| headroom.refusing()), "the headroom report says the guard is refusing");
	// The refusal is not recorded against the key: the same batch lands once there is room.
	drop(store);
	let store = store_at(dir.path(), 1).await;
	let (ts, vs) = values(1_000);
	let receipt = store.ingest(ASPECT, &IngestBatch::dense("k1", ts, vs)).await.expect("admitted under a satisfiable reserve");
	assert!(!receipt.duplicate);
	assert_eq!(store.segment_count(ASPECT).await.unwrap(), 1);
	let headroom = store.headroom().expect("described");
	drop(store);
	assert!(!headroom.refusing());
	assert_eq!(headroom.writable_bytes, headroom.available_bytes - 1);
}

#[tokio::test]
async fn a_direct_seal_below_the_reserve_writes_nothing() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), IMPOSSIBLE).await;
	let (ts, vs) = values(100);
	let err = store.seal_declared(ASPECT, &ts, &vs).await.expect_err("the guard refuses");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::InsufficientStorage { .. })), "{err:#}");
	let err = store.seal_declared_paged(ASPECT, &ts, &vs, 10).await.expect_err("the paged seal too");
	drop(store);
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::InsufficientStorage { .. })), "{err:#}");
	assert_eq!(frames(dir.path()), 0);
}

#[tokio::test]
async fn a_disabled_guard_still_reports_headroom() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), 0).await;
	let (ts, vs) = values(10);
	store.seal_declared(ASPECT, &ts, &vs).await.expect("no guard, no refusal");
	let headroom = store.headroom().expect("described");
	let guard = store.disk_guard();
	drop(store);
	assert_eq!((headroom.reserve_bytes, headroom.writable_bytes), (0, headroom.available_bytes));
	assert!(!headroom.refusing());
	assert!(!guard.is_enabled());
}

/// #656: the tests above hold an impossible reserve against a roomy disk; these fill a
/// real filesystem. Each runs its body in a child process inside an unprivileged user and
/// mount namespace (`unshare --user --map-root-user --mount`), where it mounts a small
/// tmpfs over a temporary directory, so `statvfs` reports the numbers of a disk that
/// actually runs out and the writes past it actually fail with `ENOSPC`. The mount is
/// private to the child and goes with it.
///
/// Where unprivileged user namespaces are unavailable (some CI kernels restrict them) the
/// parent says so on stderr and passes; `WEFT_REQUIRE_TMPFS=1` turns that into a
/// failure.
#[cfg(target_os = "linux")]
mod full_filesystem {
	use std::{
		ffi::CString, path::{Path, PathBuf}, process::Command
	};

	use bigdecimal::BigDecimal;
	use tempfile::TempDir;

	use super::{frames, store_at, ASPECT};
	use crate::{disk_space_at, IngestBatch, StoreError};

	/// Set in the child: the body runs only there.
	const CHILD: &str = "WEFT_TEST_TMPFS_CHILD";
	/// The tmpfs every child mounts.
	const TMPFS_BYTES: u64 = 64 * 1024 * 1024;
	/// Rows per ingest: one frame of noisy integers, near 800 KB.
	const ROWS: i64 = 100_000;
	/// More batches than the tmpfs can hold, so a loop that never stops is a failure.
	const MAX_BATCHES: i64 = 1_000;

	/// Run `child` (a test in this module) on a fresh tmpfs, or report why it cannot be.
	fn run_on_tmpfs(child: &str) {
		let exe = std::env::current_exe().expect("finds the test binary");
		let name = format!("types::segment_store::disk_guard_tests::full_filesystem::{child}");
		let out = Command::new("unshare").args(["--user", "--map-root-user", "--mount", "--"]).arg(exe).args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"]).env(CHILD, "1").output();
		let required = std::env::var_os("WEFT_REQUIRE_TMPFS").is_some_and(|v| v == "1");
		let out = match out {
			Ok(out) => out,
			Err(e) => {
				assert!(!required, "WEFT_REQUIRE_TMPFS=1 but `unshare` could not run: {e}");
				eprintln!("{child}: skipped, `unshare` could not run: {e}");
				return;
			}
		};
		let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
		if !stdout.contains("running 1 test") {
			assert!(!required, "WEFT_REQUIRE_TMPFS=1 but the namespace could not be made: {stderr}");
			eprintln!("{child}: skipped, no unprivileged user and mount namespace here: {}", stderr.trim());
			return;
		}
		assert!(out.status.success() && stdout.contains("1 passed"), "{child} failed on a full tmpfs:\n{stdout}\n{stderr}");
	}

	/// A tmpfs of `bytes` mounted over a temporary directory, unmounted on drop so the
	/// directory under it can be removed.
	struct Tmpfs {
		dir: TempDir,
	}

	impl Tmpfs {
		fn mount(bytes: u64) -> Self {
			let dir = TempDir::new().expect("tempdir");
			let target = CString::new(dir.path().as_os_str().as_encoded_bytes()).expect("no NUL in a temp path");
			let (source, fstype, data) = (c"weft-full-disk", c"tmpfs", CString::new(format!("size={bytes},mode=0700")).expect("no NUL"));
			// SAFETY: every pointer is a NUL-terminated string that outlives the call.
			let rc = unsafe { libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, data.as_ptr().cast()) };
			assert_eq!(rc, 0, "mount tmpfs at {}: {}", dir.path().display(), std::io::Error::last_os_error());
			Self { dir }
		}

		fn path(&self) -> &Path {
			self.dir.path()
		}
	}

	impl Drop for Tmpfs {
		fn drop(&mut self) {
			if let Ok(target) = CString::new(self.dir.path().as_os_str().as_encoded_bytes()) {
				// SAFETY: `target` is a NUL-terminated path that outlives the call.
				unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
			}
		}
	}

	/// Batch `batch`'s timestamps (disjoint from every other batch's) and noisy integer
	/// values, which a frame cannot compress much.
	fn noisy(batch: i64) -> (Vec<i64>, Vec<BigDecimal>) {
		let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ batch.unsigned_abs();
		(0..ROWS)
			.map(|i| {
				state ^= state << 13;
				state ^= state >> 7;
				state ^= state << 17;
				(batch * ROWS + i, BigDecimal::from(state >> 32))
			})
			.unzip()
	}

	/// Whether `err`'s chain holds the operating system's `ENOSPC`.
	fn is_enospc(err: &anyhow::Error) -> bool {
		err.chain().any(|cause| cause.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull || io.raw_os_error() == Some(libc::ENOSPC)))
	}

	/// Every row of the first `batches` batches reads back.
	async fn assert_reads_back(root: &Path, reserve: u64, batches: i64) {
		let store = store_at(root, reserve).await;
		assert_eq!(store.segment_count(ASPECT).await.unwrap(), usize::try_from(batches).unwrap(), "every admitted batch is one published segment after a reopen");
		assert_eq!(frames(root), usize::try_from(batches).unwrap(), "no frame on disk but the published ones");
		let (ts, vs) = store.read_time_range(ASPECT, 0, batches * ROWS).await.expect("reads");
		drop(store);
		assert_eq!(i64::try_from(ts.len()).unwrap(), batches * ROWS);
		for batch in [0, batches - 1] {
			let (stamps, values) = noisy(batch);
			let at = usize::try_from(batch * ROWS).unwrap();
			assert_eq!(ts[at..at + stamps.len()], stamps[..]);
			assert!(vs[at..at + values.len()].iter().zip(&values).all(|(got, want)| got.as_ref() == Some(want)), "batch {batch} reads back as written");
		}
	}

	#[test]
	fn a_guarded_store_refuses_before_a_real_filesystem_fills() {
		run_on_tmpfs("guarded_child");
	}

	#[test]
	fn an_unguarded_store_that_hits_enospc_recovers_once_there_is_room() {
		run_on_tmpfs("unguarded_child");
	}

	/// With a 16 MiB reserve on a 64 MiB tmpfs, ingest until the guard refuses: the
	/// refusal is the typed 507, it comes while the filesystem still has room, nothing
	/// half-written is left, and the refused key lands once the reserve is lowered.
	#[tokio::test]
	async fn guarded_child() {
		if std::env::var_os(CHILD).is_none() {
			return;
		}
		let tmpfs = Tmpfs::mount(TMPFS_BYTES);
		let root: PathBuf = tmpfs.path().join("store");
		let reserve = 16 * 1024 * 1024;
		let store = store_at(&root, reserve).await;
		let mut admitted = 0;
		let err = loop {
			assert!(admitted < MAX_BATCHES, "a 64 MiB filesystem took {MAX_BATCHES} batches without a refusal");
			let (ts, vs) = noisy(admitted);
			match store.ingest(ASPECT, &IngestBatch::dense(format!("k{admitted}"), ts, vs)).await {
				Ok(_) => admitted += 1,
				Err(err) => break err,
			}
		};
		assert!(!is_enospc(&err), "the guard refused before the filesystem did, not after: {err:#}");
		let Some(StoreError::InsufficientStorage { available_bytes, reserve_bytes, need_bytes }) = err.downcast_ref::<StoreError>() else {
			panic!("expected InsufficientStorage, got {err:#}");
		};
		assert_eq!(*reserve_bytes, reserve);
		assert!(*available_bytes < reserve + need_bytes && *available_bytes <= TMPFS_BYTES, "the refusal is real arithmetic on the real filesystem: {available_bytes} available");
		let space = disk_space_at(&root).expect("a tmpfs describes itself");
		eprintln!("guarded: {admitted} batches admitted, refused needing {need_bytes} with {available_bytes} available; {} bytes still available", space.available_bytes);
		assert!(admitted > 0, "the reserve leaves room for some batches");
		assert!(space.available_bytes > reserve / 2, "the filesystem kept most of the reserve: {} available", space.available_bytes);
		assert!(store.headroom().is_some_and(|headroom| headroom.refusing() || headroom.writable_bytes < *need_bytes));
		drop(store);
		assert_reads_back(&root, reserve, admitted).await;

		// The refusal was not recorded against its key: the same batch lands under a lower reserve.
		let store = store_at(&root, 1).await;
		let (ts, vs) = noisy(admitted);
		let receipt = store.ingest(ASPECT, &IngestBatch::dense(format!("k{admitted}"), ts, vs)).await.expect("admitted once the reserve is lowered");
		assert!(!receipt.duplicate);
		drop(store);
		assert_reads_back(&root, 1, admitted + 1).await;
	}

	/// With the guard off, ingest until the filesystem itself refuses: the failure carries
	/// `ENOSPC` (what the server maps to 507), publishes nothing, and once a ballast file
	/// is deleted the store reopens with every earlier batch intact, no orphan frame, and
	/// the failed key still writable.
	#[tokio::test]
	async fn unguarded_child() {
		if std::env::var_os(CHILD).is_none() {
			return;
		}
		let tmpfs = Tmpfs::mount(TMPFS_BYTES);
		let ballast = tmpfs.path().join("ballast");
		std::fs::write(&ballast, vec![0x5a_u8; 8 * 1024 * 1024]).expect("writes the ballast");
		let root: PathBuf = tmpfs.path().join("store");
		let store = store_at(&root, 0).await;
		let mut admitted = 0;
		let err = loop {
			assert!(admitted < MAX_BATCHES, "a 64 MiB filesystem took {MAX_BATCHES} batches without ENOSPC");
			let (ts, vs) = noisy(admitted);
			match store.ingest(ASPECT, &IngestBatch::dense(format!("k{admitted}"), ts, vs)).await {
				Ok(_) => admitted += 1,
				Err(err) => break err,
			}
		};
		let space = disk_space_at(&root).expect("a tmpfs describes itself");
		eprintln!("unguarded: {admitted} batches admitted, then: {err:#}; {} bytes available", space.available_bytes);
		assert!(is_enospc(&err), "the full filesystem's ENOSPC reaches the caller, where the server classifies it 507: {err:#}");
		assert!(admitted > 0);
		drop(store);

		std::fs::remove_file(&ballast).expect("frees the ballast");
		assert_reads_back(&root, 0, admitted).await;
		let store = store_at(&root, 0).await;
		let (ts, vs) = noisy(admitted);
		let receipt = store.ingest(ASPECT, &IngestBatch::dense(format!("k{admitted}"), ts, vs)).await.expect("the failed key lands once there is room");
		assert!(!receipt.duplicate, "a batch that hit ENOSPC was not recorded against its key");
		drop(store);
		assert_reads_back(&root, 0, admitted + 1).await;
	}
}
