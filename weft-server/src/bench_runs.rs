//! The benchmark-run routes (roadmap Phase 3): `GET /bench/runs` and
//! `GET /bench/runs/{id}`, serving the EVI-3 run manifests weft-bench writes.
//!
//! ## What a run id is
//!
//! A run has no id of its own: `weft-bench` writes `<report>.manifest.json`
//! beside each report and keys the run on that path. The id here is the
//! **lower-case hex SHA-256 of the manifest file's exact bytes**, so it is
//! content-addressed rather than assigned:
//!
//! - nothing new is stored, and no manifest schema changes — the manifest
//!   already records the report's own hash, so hashing the manifest pins the
//!   report too;
//! - the id is the same on every machine that holds the same file, so a
//!   published claim can cite it, and `weft-bench` prints it when it writes
//!   the manifest;
//! - an edited manifest is a different run: the old id stops resolving
//!   instead of quietly serving the new bytes. That is also why the body can
//!   be cached forever.
//!
//! ## Where runs come from
//!
//! [`BENCH_RUNS_DIR_ENV`] names a directory. Every `*.manifest.json` under it
//! (to [`MAX_DEPTH`](crate::bench_runs::MAX_DEPTH) levels, symlinks not followed, each at most
//! [`MAX_MANIFEST_BYTES`](crate::bench_runs::MAX_MANIFEST_BYTES)) is a run. The directory is scanned per request:
//! runs arrive by `weft-bench` writing files, not through this server, and a
//! cache would have to be told when that happens. Unset, both routes answer
//! `503 unconfigured`.
//!
//! ## Who may read it
//!
//! A manifest records host hardware, the toolchain and the command line —
//! redacted at capture, but still operator information. The routes ask for
//! the [metrics scope](crate::routes::Scope::Metrics), the operational
//! telemetry scope `/metrics` and `/debug/profile/current` already use, so
//! with auth on only a maintainer or admin key reads them, and SEC-8's
//! exposure matrix covers them like the rest of that scope. They register at
//! the [experimental tier](crate::settings::Tier::Experimental): the surface is
//! not part of the 1.0 freeze.

use std::{
	fmt::Write as _, path::{Path, PathBuf}, sync::Arc
};

use axum::{
	extract::{Path as UrlPath, State}, http::{header, HeaderValue}, response::{IntoResponse, Response}, Json
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{error::ApiError, state::AppState};

/// Environment variable naming the directory of run manifests.
pub const BENCH_RUNS_DIR_ENV: &str = "WEFT_BENCH_RUNS_DIR";

/// How many directory levels below [`BENCH_RUNS_DIR_ENV`] are searched. A
/// nightly layout (`<date>/<profile>/…`) fits; a mount of a whole home
/// directory is not walked to the bottom on every request.
pub const MAX_DEPTH: usize = 4;

/// The largest file read as a manifest. Real manifests are a few KiB; a file
/// past this is not one, and is skipped rather than read into memory.
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

/// The manifest suffix `weft-bench` writes (`main.rs`, `manifest_path_for`).
const MANIFEST_SUFFIX: &str = ".manifest.json";

/// The configured run directory.
#[derive(Debug, Clone)]
pub struct BenchRuns {
	dir: PathBuf,
}

/// One run in the listing: its id, where it sits under the run directory,
/// and the few manifest fields an operator picks a run by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSummary {
	/// The run id: SHA-256 of the manifest's bytes, lower-case hex.
	pub id: String,
	/// The manifest's path relative to the run directory, `/`-separated.
	pub path: String,
	/// The manifest's `schema_version`, when it has one.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub schema_version: Option<u64>,
	/// When the run finished (the manifest's `generated_at`).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub generated_at: Option<String>,
	/// The `--claim` the run is evidence for, if labelled as one.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub claim: Option<String>,
}

/// The body of `GET /bench/runs`.
#[derive(Debug, Clone, Serialize)]
pub struct RunListResponse {
	/// Every run found, newest `generated_at` first.
	pub runs: Vec<RunSummary>,
}

/// The run id of a manifest's bytes: their SHA-256, lower-case hex.
#[must_use]
pub fn run_id(bytes: &[u8]) -> String {
	let digest = Sha256::digest(bytes);
	let mut id = String::with_capacity(64);
	for byte in digest {
		let _ = write!(id, "{byte:02x}");
	}
	id
}

/// Whether `id` is shaped like a run id (64 lower-case hex digits). Checked
/// before any file is touched, so a path in the URL never reaches the walk.
#[must_use]
pub fn is_run_id(id: &str) -> bool {
	id.len() == 64 && id.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

impl BenchRuns {
	/// Serve the manifests under `dir`.
	pub fn new(dir: impl Into<PathBuf>) -> Self {
		Self { dir: dir.into() }
	}

	/// The run directory.
	#[must_use]
	pub fn dir(&self) -> &Path {
		&self.dir
	}

	/// Every manifest file under the run directory, as (absolute path, path
	/// relative to the directory). A subdirectory that cannot be read is
	/// skipped; the root itself not being readable is the error.
	fn manifest_files(&self) -> std::io::Result<Vec<(PathBuf, PathBuf)>> {
		let mut found = Vec::new();
		let mut pending = vec![(self.dir.clone(), 0usize)];
		let mut root = true;
		while let Some((directory, depth)) = pending.pop() {
			let entries = match std::fs::read_dir(&directory) {
				Ok(entries) => entries,
				Err(error) if root => return Err(error),
				Err(_) => continue,
			};
			root = false;
			for entry in entries.flatten() {
				// `file_type` does not follow symlinks, so a link out of the
				// run directory is never walked or read.
				let Ok(kind) = entry.file_type() else { continue };
				let path = entry.path();
				if kind.is_dir() {
					if depth < MAX_DEPTH {
						pending.push((path, depth + 1));
					}
				} else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(MANIFEST_SUFFIX) {
					let relative = path.strip_prefix(&self.dir).map(Path::to_path_buf).unwrap_or_default();
					found.push((path, relative));
				}
			}
		}
		found.sort_by(|a, b| a.1.cmp(&b.1));
		Ok(found)
	}

	/// The bytes of one manifest file, or `None` when it is too large to be one
	/// or cannot be read.
	fn read_manifest(path: &Path) -> Option<Vec<u8>> {
		let length = std::fs::symlink_metadata(path).ok()?.len();
		if length > MAX_MANIFEST_BYTES {
			return None;
		}
		std::fs::read(path).ok()
	}

	/// Every run under the directory, newest first.
	///
	/// # Errors
	///
	/// The run directory itself cannot be read.
	pub fn list(&self) -> std::io::Result<Vec<RunSummary>> {
		let mut runs: Vec<RunSummary> = self
			.manifest_files()?
			.into_iter()
			.filter_map(|(path, relative)| {
				let bytes = Self::read_manifest(&path)?;
				let fields: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
				let text = |name: &str| fields.get(name).and_then(serde_json::Value::as_str).map(str::to_string);
				Some(RunSummary { id: run_id(&bytes), path: relative.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"), schema_version: fields.get("schema_version").and_then(serde_json::Value::as_u64), generated_at: text("generated_at"), claim: text("claim") })
			})
			.collect();
		// RFC 3339 UTC strings sort as times; a run without one sorts last.
		runs.sort_by(|a, b| b.generated_at.cmp(&a.generated_at).then_with(|| a.path.cmp(&b.path)));
		Ok(runs)
	}

	/// The bytes of the manifest whose run id is `id`, if one is here.
	///
	/// # Errors
	///
	/// The run directory itself cannot be read.
	pub fn find(&self, id: &str) -> std::io::Result<Option<Vec<u8>>> {
		Ok(self.manifest_files()?.into_iter().find_map(|(path, _)| Self::read_manifest(&path).filter(|bytes| run_id(bytes) == id)))
	}
}

/// The configured run directory, or the `503` that says it is not.
fn configured(state: &AppState) -> Result<Arc<BenchRuns>, ApiError> {
	state.bench_runs().cloned().ok_or_else(|| ApiError::unconfigured_subsystem("bench_runs", BENCH_RUNS_DIR_ENV))
}

/// Run blocking file-system work off the async workers.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> std::io::Result<T> + Send + 'static) -> Result<T, ApiError> {
	match tokio::task::spawn_blocking(work).await {
		Ok(Ok(value)) => Ok(value),
		Ok(Err(error)) => Err(ApiError::internal(format!("cannot read the bench run directory: {error}"))),
		Err(error) => Err(ApiError::internal(format!("the bench run scan did not finish: {error}"))),
	}
}

/// Handle `GET /bench/runs`: every run manifest under the run directory.
///
/// # Errors
///
/// `503 unconfigured` without [`BENCH_RUNS_DIR_ENV`]; `500 internal` when the
/// directory cannot be read.
pub async fn list_runs(State(state): State<AppState>) -> Result<Json<RunListResponse>, ApiError> {
	let runs = configured(&state)?;
	let listed = blocking(move || runs.list()).await?;
	Ok(Json(RunListResponse { runs: listed }))
}

/// Handle `GET /bench/runs/{id}`: the manifest with that run id, byte for
/// byte, so a client can re-hash the body and get the id back.
///
/// # Errors
///
/// `400 bad_request` for an id that is not 64 lower-case hex digits,
/// `404 run_not_found` when no manifest hashes to it, `503 unconfigured`
/// without [`BENCH_RUNS_DIR_ENV`], `500 internal` when the directory cannot be
/// read.
pub async fn get_run(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Result<Response, ApiError> {
	let runs = configured(&state)?;
	if !is_run_id(&id) {
		return Err(ApiError::bad_request(format!("`{}` is not a run id; a run id is the 64 lower-case hex digits of the manifest's SHA-256, as `GET /bench/runs` lists them", id.chars().take(80).collect::<String>())));
	}
	let wanted = id.clone();
	let Some(bytes) = blocking(move || runs.find(&wanted)).await? else {
		return Err(ApiError::run_not_found(format!("no run manifest under the bench run directory hashes to {id}; `GET /bench/runs` lists the runs it holds")));
	};
	let mut response = bytes.into_response();
	let headers = response.headers_mut();
	headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
	// Content-addressed: these bytes are this id forever.
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
	if let Ok(etag) = HeaderValue::from_str(&format!("\"{id}\"")) {
		headers.insert(header::ETAG, etag);
	}
	Ok(response)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_run_id_is_the_sha256_of_the_bytes() {
		// SHA-256 of the empty string, the standard test vector.
		assert_eq!(run_id(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
		assert!(is_run_id(&run_id(b"{}")));
	}

	#[test]
	fn only_64_lower_case_hex_digits_are_a_run_id() {
		assert!(is_run_id(&"a".repeat(64)));
		let (upper, non_hex, short, long) = ("A".repeat(64), "g".repeat(64), "a".repeat(63), "a".repeat(65));
		for id in ["", "abc", &upper, &non_hex, &short, &long, "../../etc/passwd"] {
			assert!(!is_run_id(id), "{id:?}");
		}
	}

	#[test]
	fn the_walk_finds_manifests_and_skips_everything_else() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join("a.manifest.json"), br#"{"schema_version":23,"generated_at":"2026-11-01T00:00:00Z"}"#).unwrap();
		std::fs::write(dir.path().join("a.json"), b"{}").unwrap();
		std::fs::create_dir_all(dir.path().join("nightly/2026-11-02")).unwrap();
		std::fs::write(dir.path().join("nightly/2026-11-02/b.manifest.json"), br#"{"generated_at":"2026-11-02T00:00:00Z","claim":"p99"}"#).unwrap();
		let deep = (0..=MAX_DEPTH).fold(dir.path().to_path_buf(), |path, level| path.join(format!("d{level}")));
		std::fs::create_dir_all(&deep).unwrap();
		std::fs::write(deep.join("too-deep.manifest.json"), b"{}").unwrap();
		let runs = BenchRuns::new(dir.path()).list().unwrap();
		let paths: Vec<&str> = runs.iter().map(|run| run.path.as_str()).collect();
		assert_eq!(paths, ["nightly/2026-11-02/b.manifest.json", "a.manifest.json"], "newest first, the report and the too-deep file left out");
		assert_eq!(runs[0].claim.as_deref(), Some("p99"));
		assert_eq!(runs[1].schema_version, Some(23));
	}

	#[cfg(unix)]
	#[test]
	fn a_symlink_out_of_the_run_directory_is_not_followed() {
		let outside = tempfile::tempdir().unwrap();
		std::fs::write(outside.path().join("secret.manifest.json"), b"{}").unwrap();
		let dir = tempfile::tempdir().unwrap();
		std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
		std::os::unix::fs::symlink(outside.path().join("secret.manifest.json"), dir.path().join("file.manifest.json")).unwrap();
		assert!(BenchRuns::new(dir.path()).list().unwrap().is_empty());
	}

	#[test]
	fn find_resolves_an_id_to_the_exact_bytes() {
		let dir = tempfile::tempdir().unwrap();
		let bytes = br#"{"generated_at":"2026-11-01T00:00:00Z"}"#;
		std::fs::write(dir.path().join("r.manifest.json"), bytes).unwrap();
		let runs = BenchRuns::new(dir.path());
		assert_eq!(runs.find(&run_id(bytes)).unwrap().as_deref(), Some(&bytes[..]));
		assert_eq!(runs.find(&run_id(b"other")).unwrap(), None);
	}

	#[test]
	fn a_missing_run_directory_is_an_error_not_an_empty_list() {
		let dir = tempfile::tempdir().unwrap();
		assert!(BenchRuns::new(dir.path().join("absent")).list().is_err());
	}

	// ── over HTTP, through the sealed registry ───────────────────────────────

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tower::ServiceExt as _;

	use crate::auth::{format_key_hash, AuthConfig, KeyStore};

	const MANIFEST: &[u8] = br#"{"schema_version":23,"generated_at":"2026-11-03T00:00:00Z","claim":"interp-latency"}"#;

	async fn get(router: axum::Router, uri: &str, token: Option<&str>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
		let mut request = Request::builder().uri(uri);
		if let Some(token) = token {
			request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
		}
		let response = router.oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
		let (parts, body) = response.into_parts();
		(parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
	}

	fn json(bytes: &[u8]) -> serde_json::Value {
		serde_json::from_slice(bytes).unwrap()
	}

	fn runs_dir() -> tempfile::TempDir {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join("interp__weftdb.manifest.json"), MANIFEST).unwrap();
		dir
	}

	#[tokio::test]
	async fn without_a_run_directory_both_routes_say_which_variable_enables_them() {
		for uri in ["/bench/runs".to_string(), format!("/bench/runs/{}", run_id(MANIFEST))] {
			let (status, _, body) = get(crate::app(), &uri, None).await;
			assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
			let body = json(&body);
			assert_eq!(body["code"], "unconfigured", "{body}");
			assert_eq!(body["details"]["enabled_by"], BENCH_RUNS_DIR_ENV, "{body}");
		}
	}

	#[tokio::test]
	async fn a_run_is_listed_then_served_byte_for_byte_under_its_hash() {
		let dir = runs_dir();
		let router = crate::app_with_state(AppState::new().with_bench_runs(BenchRuns::new(dir.path())));
		let (status, _, body) = get(router.clone(), "/bench/runs", None).await;
		assert_eq!(status, StatusCode::OK);
		let listed = json(&body);
		let id = listed["runs"][0]["id"].as_str().unwrap().to_string();
		assert_eq!(id, run_id(MANIFEST));
		assert_eq!(listed["runs"][0]["claim"], "interp-latency");
		assert_eq!(listed["runs"][0]["path"], "interp__weftdb.manifest.json");

		let (status, headers, body) = get(router, &format!("/bench/runs/{id}"), None).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body, MANIFEST, "the exact bytes, so a client can re-hash them");
		assert_eq!(headers[header::CONTENT_TYPE], "application/json");
		assert_eq!(headers[header::ETAG], format!("\"{id}\"").as_str());
	}

	#[tokio::test]
	async fn an_unknown_id_is_a_404_and_a_malformed_one_a_400() {
		let dir = runs_dir();
		let router = crate::app_with_state(AppState::new().with_bench_runs(BenchRuns::new(dir.path())));
		let (status, _, body) = get(router.clone(), &format!("/bench/runs/{}", run_id(b"edited")), None).await;
		assert_eq!(status, StatusCode::NOT_FOUND);
		assert_eq!(json(&body)["code"], "run_not_found");
		let upper = "F".repeat(64);
		for bad in ["abc", "..%2F..%2Fetc%2Fpasswd", &upper] {
			let (status, _, body) = get(router.clone(), &format!("/bench/runs/{bad}"), None).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
			assert_eq!(json(&body)["code"], "bad_request");
		}
	}

	#[tokio::test]
	async fn with_auth_on_only_the_metrics_scope_reads_runs() {
		const READER: &str = "weft_reader_token_for_bench_runs";
		const MAINTAINER: &str = "weft_maintainer_token_for_bench_runs";
		let keys = format!("{{\"version\":1,\"keys\":[{{\"id\":\"r\",\"role\":\"reader\",\"key_hash\":\"{}\"}},{{\"id\":\"m\",\"role\":\"maintainer\",\"key_hash\":\"{}\"}}]}}", format_key_hash(READER), format_key_hash(MAINTAINER));
		let dir = runs_dir();
		let router = crate::app_with_state(AppState::new().with_bench_runs(BenchRuns::new(dir.path())).with_auth(AuthConfig::with_keys(KeyStore::parse(&keys).unwrap())));
		let uri = format!("/bench/runs/{}", run_id(MANIFEST));
		assert_eq!(get(router.clone(), &uri, None).await.0, StatusCode::UNAUTHORIZED);
		assert_eq!(get(router.clone(), &uri, Some(READER)).await.0, StatusCode::FORBIDDEN, "host hardware and command lines are not a reader's");
		assert_eq!(get(router.clone(), "/bench/runs", Some(READER)).await.0, StatusCode::FORBIDDEN);
		assert_eq!(get(router, &uri, Some(MAINTAINER)).await.0, StatusCode::OK);
	}

	#[test]
	fn the_routes_register_experimental_under_the_metrics_scope() {
		let table = crate::routes::build_registry().table();
		for operation in ["list_bench_runs", "get_bench_run"] {
			let entry = table.by_operation(operation).unwrap_or_else(|| panic!("{operation} is registered"));
			assert_eq!(entry.spec.scope, crate::routes::Scope::Metrics);
			assert_eq!(entry.spec.tier, crate::settings::Tier::Experimental);
		}
	}
}
