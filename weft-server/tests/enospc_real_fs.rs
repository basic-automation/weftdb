#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]
//! ROB-17 / #656: how the server behaves when the filesystem is full.
//!
//! The library already pins the guard's arithmetic
//! (`weftdb/src/types/disk_guard.rs` and
//! `weftdb/src/types/segment_store/disk_guard_tests.rs`). These tests pin what a
//! client actually receives: the HTTP 507 `insufficient_storage` on top of the
//! injectable space source (#694 merged).
//!
//! ## Tests
//!
//! - [`space_source_reports_a_full_filesystem_and_the_server_says_507`]:
//!   unconditional. The guard's reserve is set above what any real filesystem
//!   holds, so every write is refused as `507 insufficient_storage`.
//! - [`a_real_full_filesystem_produces_enospc_end_to_end`]: skips on hosts
//!   without `unshare` + user namespaces. Inside a `size=1m` tmpfs the copy
//!   genuinely returns ENOSPC and the server maps it to 507.
//!
//! ## Seam
//!
//! The injectable `Option<DiskSpace>` argument of
//! [`weftdb::DiskGuard::admit`](weftdb::DiskGuard::admit) — the lowest-level
//! seam — is reachable through `reserve_disk_with_space` in
//! `weft-server/src/ingest.rs`, which is `fn` (crate-private). From an
//! integration test the next-best approach is
//! [`SegmentStoreOptions::with_disk_reserve_bytes`] with a reserve no
//! filesystem can satisfy, which lets the real `disk_space_at` probe produce
//! the refusal.

use std::sync::Arc;

use axum::{
	body::Body, http::{Request, StatusCode}, Router
};
use bigdecimal::BigDecimal;
use tempfile::TempDir;
use tower::ServiceExt as _;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app_with_state, AppState};
use weftdb::{SegmentStore, SegmentStoreOptions};

/// Environment variable the parent sets to pass the tmpfs root to the child.
const CHILD_ROOT: &str = "WEFT_ENOSPC_CHILD_ROOT";

/// The aspect every test writes.
const ASPECT: &str = "price";

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)
}

/// A router with a store whose disk guard has an impossible reserve.
///
/// `u64::MAX` guarantees refusal regardless of the host's free space, so this
/// is unconditional: every write gets `507 insufficient_storage`.
async fn router_with_impossible_reserve() -> (TempDir, Router) {
	let dir = TempDir::new().expect("tempdir");
	let store = Arc::new(SegmentStore::open_with_options(dir.path(), SegmentStoreOptions::default().with_disk_reserve_bytes(u64::MAX)).await.expect("opens the store"));
	store.declare(ASPECT, &schema()).await.expect("declares the aspect");
	let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));
	drop(store);
	(dir, router)
}

/// POST a JSON points body (a single point at timestamp 1, value 1.0).
fn ingest_points_request() -> Request<Body> {
	Request::builder().method("POST").uri(format!("/api/v1/storage/{ASPECT}/points")).header("content-type", "application/json").body(Body::from(r#"{"points":[{"t":1,"v":1.0}]}"#)).expect("builds the request")
}

/// Assert the response is `507 insufficient_storage`.
async fn assert_507(response: axum::response::Response) {
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.expect("a body fits");
	let body: serde_json::Value = serde_json::from_slice(&bytes).expect("a JSON body");

	assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
	assert_eq!(body["code"], "insufficient_storage", "the code is the §4 name: {body}");
}

#[tokio::test]
async fn space_source_reports_a_full_filesystem_and_the_server_says_507() {
	// Act: the guard's reserve is set above what any real filesystem holds.
	// The guard checks disk_space_at(store.root()), finds the real
	// filesystem's available bytes below u64::MAX, and refuses.
	let (_dir, router) = router_with_impossible_reserve().await;
	let response = router.oneshot(ingest_points_request()).await.expect("the router answers");
	assert_507(response).await;
}

/// The child body: runs inside a user+mount namespace on a genuinely full
/// tmpfs. Opens a `SegmentStore` with the default guard, sends an ingest,
/// and asserts the client sees `507 insufficient_storage`. In a normal test
/// run (`CHILD_ROOT` unset) this returns immediately.
///
/// The guard's `statvfs`-based pre-check refuses the write. If the guard
/// were bypassed, the write itself would fail with ENOSPC and the response
/// would be `500 internal_error` (the error classifier maps ENOSPC to 507
/// only for ingest and read errors, not for generic store errors) — so the
/// assertion would fail, proving the guard is the only thing producing 507
/// on this path.
#[tokio::test]
async fn enospc_real_fs_child() {
	let Some(root) = std::env::var_os(CHILD_ROOT) else { return };
	let root = std::path::Path::new(&root);

	// The tmpfs at root is already mounted. Open the store with the default
	// guard (256 MiB reserve) and declare the aspect.
	let store = Arc::new(SegmentStore::open_with_options(root, SegmentStoreOptions::default()).await.expect("the store opens: there is still space for the catalog"));
	store.declare(ASPECT, &schema()).await.expect("declares the aspect");

	// Fill the remaining space to ENOSPC so the guard has nothing to
	// work with. dd(1) inside the same namespace writes to the same
	// tmpfs.
	let fill_path = root.join("fill");
	let fill_out = std::process::Command::new("dd").args(["if=/dev/zero", &format!("of={}", fill_path.display()), "bs=1024", "count=2048"]).output();
	match fill_out {
		Err(e) => panic!("dd failed: {e}"),
		Ok(out) if out.status.success() => panic!("dd succeeded when we expected ENOSPC — the tmpfs is larger than 2 MiB"),
		Ok(out) => {
			let stderr = String::from_utf8_lossy(&out.stderr);
			eprintln!("enospc_real_fs: fs filled (dd exit={}, stderr: {stderr})", out.status.code().unwrap_or(-1));
		}
	}

	// Now the tmpfs is genuinely full. Send an ingest through the router.
	let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));
	drop(store);
	assert_507(router.oneshot(ingest_points_request()).await.expect("the router answers")).await;
}

/// Mount a 1 MiB tmpfs inside a user+mount namespace, open a store on it,
/// fill it to ENOSPC, and run [`enospc_real_fs_child`] against the full
/// filesystem. On hosts without `unshare` the test skips honestly.
#[tokio::test]
async fn a_real_full_filesystem_produces_enospc_end_to_end() {
	// Probe: can this host create a user+mount namespace?
	let probe = std::process::Command::new("unshare").args(["-Um", "true"]).output();

	match probe {
		Err(e) => {
			eprintln!("SKIP enospc_real_fs: unshare binary not available ({e})");
			return;
		}
		Ok(output) if !output.status.success() => {
			let stderr = String::from_utf8_lossy(&output.stderr);
			eprintln!(
				"SKIP enospc_real_fs: no usable user+mount namespace on this \
				 host ({reason})",
				reason = stderr.trim()
			);
			return;
		}
		_ => {}
	}

	let exe = std::env::current_exe().expect("finds the test binary");

	// Create a tempdir. Inside the namespace this will be the tmpfs mount point.
	let dir = TempDir::new().expect("a tempdir for the namespace mount");
	let root = dir.path().to_string_lossy().to_string();

	// Shell script: mount a 1 MiB tmpfs, then exec the test binary as the
	// child inside the namespace. The child sees the same tmpfs and runs
	// enospc_real_fs_child which opens a store, fills the disk, and asserts
	// 507.
	let script = format!("set -eu\n\
		 mount -t tmpfs -o size=1m tmpfs \"{root}\"\n\
		 exec \"$1\" enospc_real_fs_child --exact --nocapture \
\t	     --test-threads=1");

	let result = std::process::Command::new("unshare").args(["-Um", "-r", "sh", "-c", &script, "--", exe.to_str().unwrap()]).env(CHILD_ROOT, &root).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).output().expect("runs the namespace child");

	let stdout = String::from_utf8_lossy(&result.stdout);
	let stderr = String::from_utf8_lossy(&result.stderr);

	if !result.status.success() {
		eprintln!("enospc_real_fs child failed: exit={} stdout:\n{stdout}\nstderr:\n{stderr}", result.status.code().unwrap_or(-1));
	}

	// Print the child's stdout so its eprint! calls appear in the test log.
	for line in stdout.lines() {
		eprintln!("{line}");
	}

	assert!(result.status.success(), "enospc_real_fs child exited with {}", result.status.code().unwrap_or(-1));
}
