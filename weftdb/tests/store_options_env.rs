//! Release plan C-1: the library reads no `WEFT_*` variable on its own. A `SegmentStore`
//! opened with `open` or `open_scoped` is configured by `SegmentStoreOptions::default()`,
//! whatever the environment says; an embedder that wants the environment's settings asks
//! for them with `SegmentStoreOptions::from_env()` (as `weft-server` does) and passes them
//! to `open_with_options`. `WEFT_ON_AMBIGUOUS_COMMIT` is the server's to read: the
//! library never exits the process.
//!
//! The environment test is the only code in this binary touching these variables (the
//! other test only reads source files), so no test races it on the environment.

use weftdb::{CheckpointPolicy, PartialSidecarPolicy, Resolution, SegmentStore, SegmentStoreOptions, TransposedPolicy};

#[tokio::test]
async fn the_environment_configures_a_store_only_through_explicit_options() {
	std::env::set_var("WEFT_SEGMENT_CHECKPOINT_STRIDE", "1024");
	std::env::set_var("WEFT_SEGMENT_CHECKPOINT_MIN_ROWS", "16");
	std::env::set_var("WEFT_SEGMENT_PARTIAL_BASE", "minutes");
	std::env::set_var("WEFT_ON_AMBIGUOUS_COMMIT", "exit");
	let dir = tempfile::tempdir().expect("tempdir");

	let plain = SegmentStore::open(dir.path().join("plain")).await.expect("opens");
	let plain_policies = (plain.checkpoint_policy(), plain.partial_sidecar_policy(), plain.transposed_policy());
	drop(plain);
	let scoped = SegmentStore::open_scoped(dir.path().join("scoped"), "market", "BTCUSD").await.expect("opens");
	let scoped_policies = (scoped.checkpoint_policy(), scoped.partial_sidecar_policy(), scoped.transposed_policy());
	drop(scoped);
	let configured = SegmentStore::open_with_options(dir.path().join("configured"), SegmentStoreOptions::from_env()).await.expect("opens");
	let configured_policies = (configured.checkpoint_policy(), configured.partial_sidecar_policy());
	drop(configured);
	let explicit = SegmentStoreOptions::default().with_checkpoints(CheckpointPolicy { stride: Some(64), ..CheckpointPolicy::DISABLED });
	let given = SegmentStore::open_with_options(dir.path().join("given"), explicit).await.expect("opens");
	let given_policy = given.checkpoint_policy();
	drop(given);
	for var in ["WEFT_SEGMENT_CHECKPOINT_STRIDE", "WEFT_SEGMENT_CHECKPOINT_MIN_ROWS", "WEFT_SEGMENT_PARTIAL_BASE", "WEFT_ON_AMBIGUOUS_COMMIT"] {
		std::env::remove_var(var);
	}

	let defaults = (CheckpointPolicy::DISABLED, PartialSidecarPolicy::DISABLED, TransposedPolicy::DISABLED);
	assert_eq!(plain_policies, defaults, "open ignores the environment");
	assert_eq!(scoped_policies, defaults, "open_scoped ignores the environment");
	assert_eq!(configured_policies.0, CheckpointPolicy { stride: Some(1024), min_rows: 16, ..CheckpointPolicy::DISABLED }, "from_env reads the checkpoint variables when asked to");
	assert_eq!(configured_policies.1, PartialSidecarPolicy::at(Resolution::Minutes, weftdb::DEFAULT_PARTIAL_SIDECAR_MIN_ROWS), "and the partial-sidecar ones");
	assert_eq!(given_policy.stride, Some(64), "explicit options are what the store runs with");
}

/// The library never calls `process::exit` (release plan C-1): the only process-ending
/// call in `weftdb/src` is the fault harness's deliberate `abort`. A scan, so that a new
/// call fails here rather than in a deployment.
#[test]
fn the_library_source_never_exits_the_process() {
	fn scan(dir: &std::path::Path, hits: &mut Vec<String>) {
		for entry in std::fs::read_dir(dir).expect("lists the source") {
			let path = entry.expect("an entry").path();
			if path.is_dir() {
				scan(&path, hits);
			} else if path.extension().is_some_and(|ext| ext == "rs") {
				let text = std::fs::read_to_string(&path).expect("reads a source file");
				for (n, line) in text.lines().enumerate() {
					let code = line.split("//").next().unwrap_or_default();
					if code.contains("process::exit(") || code.contains("exit(70)") {
						hits.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
					}
				}
			}
		}
	}
	let mut hits = Vec::new();
	scan(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut hits);
	assert!(hits.is_empty(), "weftdb must not exit the process; the embedder decides: {hits:#?}");
}
