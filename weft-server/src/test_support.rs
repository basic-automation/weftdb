//! Shared pieces the write-path endpoint tests are built from: a router over a
//! temporary store, and the request helpers those tests post through it.

#![allow(clippy::missing_panics_doc)]

use std::{str::FromStr, sync::Arc};

use axum::{
	body::Body, http::{Request, StatusCode}
};
use bigdecimal::BigDecimal;
use tempfile::TempDir;
use tower::ServiceExt;
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

use crate::{app_with_state, AppState};

pub(crate) async fn router_with_empty_store(dir: &TempDir) -> axum::Router {
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
	app_with_state(AppState::new().with_store(store))
}

/// POST `body` (a JSON value) to `uri`, returning the status and parsed JSON body.
pub(crate) async fn post_json(router: axum::Router, uri: &str, body: &serde_json::Value) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(body).unwrap())).unwrap()).await.unwrap();
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// Every `.weftseg`/`.weftpart` file anywhere under `dir`.
pub(crate) fn frames_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
	let mut found = Vec::new();
	for entry in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
		let path = entry.path();
		if path.is_dir() {
			found.extend(frames_under(&path));
		} else if path.extension().is_some_and(|ext| ext == "weftseg" || ext == "weftpart") {
			found.push(path);
		}
	}
	found
}

/// Declare `price` (F64, seconds) in a fresh store under `dir`, returning a router
/// over that store.
pub(crate) async fn router_with_declared_price(dir: &TempDir) -> axum::Router {
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
	store.declare("price", &weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds)).await.expect("declares");
	app_with_state(AppState::new().with_store(store))
}

/// POST `uri` with an empty body, returning the status and parsed JSON body.
pub(crate) async fn post_empty(router: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(Request::builder().method("POST").uri(uri).body(Body::empty()).unwrap()).await.unwrap();
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// Reopen a router over an existing store dir (no re-declaration — the catalog
/// persists), for reading back what a prior request sealed.
pub(crate) async fn router_with_declared_price_reopened(dir: &TempDir) -> axum::Router {
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("reopens store"));
	app_with_state(AppState::new().with_store(store))
}

/// POST a `text/plain` ILP payload to `uri`, returning status and parsed JSON.
pub(crate) async fn post_text(router: axum::Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "text/plain").body(Body::from(body.to_string())).unwrap()).await.unwrap();
	let status = response.status();
	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// Declare `temp` (F64, seconds) in a fresh store under `dir`.
pub(crate) async fn router_with_declared_temp(dir: &TempDir) -> axum::Router {
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
	store.declare("temp", &weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds)).await.expect("declares");
	app_with_state(AppState::new().with_store(store))
}

/// Reopen a router over the store dir for reading back ILP-sealed data.
pub(crate) async fn router_with_declared_temp_reopened(dir: &TempDir) -> axum::Router {
	let store = Arc::new(SegmentStore::open(dir.path()).await.expect("reopens store"));
	app_with_state(AppState::new().with_store(store))
}

/// Fetch and parse a JSON GET response from a router.

pub(crate) async fn get_json(router: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();

	let status = response.status();

	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();

	(status, serde_json::from_slice(&bytes).unwrap())
}

pub(crate) async fn post_parquet(router: axum::Router, uri: &str, body: Vec<u8>) -> (StatusCode, serde_json::Value) {
	let response = router.oneshot(Request::builder().method("POST").uri(uri).body(Body::from(body)).unwrap()).await.unwrap();

	let status = response.status();

	let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();

	let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

	(status, json)
}

/// Build a Parquet file body carrying the given columns under an F64/seconds

/// schema, the way an external producer would.

pub(crate) fn price_parquet_body(timestamps: &[i64], values: &[Option<BigDecimal>]) -> Vec<u8> {
	use weft_arrow::{columns_to_record_batch_typed, write_parquet};

	let batch = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, timestamps, values);

	write_parquet(std::slice::from_ref(&batch)).expect("writes parquet")
}

/// Build a router over a store with `price` declared (F64, seconds) but no rows

/// sealed — the starting point for an ingest test.

pub(crate) async fn router_with_declared_empty_price(dir: &TempDir) -> axum::Router {
	let store = SegmentStore::open(dir.path()).await.expect("opens store");

	store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares aspect");

	app_with_state(AppState::new().with_store(Arc::new(store)))
}

pub(crate) fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("test literal parses")
}
