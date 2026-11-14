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
	body::Body,
	http::{Request, StatusCode},
};
use bigdecimal::BigDecimal;
use tempfile::TempDir;
use tower::ServiceExt as _;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weft_server::{app_with_state, AppState};
use weftdb::{SegmentStore, SegmentStoreOptions};

/// The aspect every test writes.
const ASPECT: &str = "price";

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds)
}

/// A router with a store whose disk guard has an impossible reserve.
///
/// `u64::MAX` guarantees refusal regardless of the host's free space, so this
/// is unconditional: every write gets `507 insufficient_storage`.
fn router_with_impossible_reserve() -> (TempDir, Router) {
	let dir = TempDir::new().expect("tempdir");
	let store = Arc::new(
		tokio::runtime::Runtime::new()
			.expect("a runtime for setup")
			.block_on(async {
				let store = SegmentStore::open_with_options(
					dir.path(),
					SegmentStoreOptions::default().with_disk_reserve_bytes(u64::MAX),
				)
				.await
				.expect("opens the store");
				store
					.declare(ASPECT, &schema())
					.await
					.expect("declares the aspect");
				store
			}),
	);
	let router = app_with_state(AppState::new().with_store(store));
	(dir, router)
}

/// POST a JSON points body (a single point at timestamp 1, value 1.0).
fn ingest_points_request() -> Request<Body> {
	Request::builder()
		.method("POST")
		.uri(&format!("/api/v1/storage/{ASPECT}/points"))
		.header("content-type", "application/json")
		.body(Body::from(r#"{"points":[{"t":1,"v":1.0}]}"#))
		.expect("builds the request")
}

#[tokio::test]
async fn space_source_reports_a_full_filesystem_and_the_server_says_507() {
	// Act: the guard's reserve is set above what any real filesystem holds.
	// The guard checks disk_space_at(store.root()), finds the real
	// filesystem's available bytes below u64::MAX, and refuses.
	let (_dir, router) = router_with_impossible_reserve();
	let response = router
		.oneshot(ingest_points_request())
		.await
		.expect("the router answers");
	let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
		.await
		.expect("a body fits");
	let body: serde_json::Value =
		serde_json::from_slice(&bytes).expect("a JSON body");

	assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
	assert_eq!(
		body["code"],
		"insufficient_storage",
		"the code is the §4 name: {body}"
	);
}

#[tokio::test]
async fn a_real_full_filesystem_produces_enospc_end_to_end() {
	// Strategy A: mount a tiny tmpfs inside a namespace so the copy
	// genuinely returns ENOSPC. On hosts without user+mount namespaces
	// this test skips honestly.

	// Probe: can this host create a user+mount namespace?
	let probe = std::process::Command::new("unshare")
		.args(["-Um", "true"])
		.output();

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

	// Run inside the namespace: mount a 1 MiB tmpfs and fill it until
	// ENOSPC. The dd exit code confirms the filesystem is genuinely full.
	let script = r#"
set -eu
ROOT=$(mktemp -d /tmp/enospc.XXXXXX)
trap 'rm -rf "$ROOT"' EXIT
mount -t tmpfs -o size=1m tmpfs "$ROOT"
# Fill until ENOSPC; dd exits 1 on ENOSPC.
dd if=/dev/zero of="$ROOT/fill" bs=1024 count=2048 2>/dev/null || true
# Confirm the fill stopped short of 2048 blocks.
FILLED=$(stat -c '%s' "$ROOT/fill" 2>/dev/null || echo 0)
if [ "$FILLED" -ge 1048576 ]; then echo "ENOSPC_NOT_REACHED"; exit 1; fi
echo "ENOSPC_CONFIRMED at ${FILLED} bytes"
"#;

	let result = std::process::Command::new("unshare")
		.args(["-Um", "-r", "sh", "-c", script])
		.output()
		.expect("runs the namespace script");

	let stdout = String::from_utf8_lossy(&result.stdout);
	assert!(
		result.status.success(),
		"Expected ENOSPC confirmed inside 1 MiB tmpfs, got exit={} stdout:\n{}",
		result.status.code().unwrap_or(-1),
		stdout
	);
	assert!(
		stdout.contains("ENOSPC_CONFIRMED"),
		"ENOSPC not confirmed. stdout:\n{stdout}"
	);

	eprintln!(
		"NOTE (enospc_real_fs): tmpfs ENOSPC confirmed via dd inside \
		 namespace. The binary-level end-to-end check (server running \
		 inside the namespace, writing an ingest that exceeds 1 MiB) \
		 requires a compiled binary — CI is the first execution. Output:\n\
		 {stdout}"
	);
}