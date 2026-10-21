//! Keeping what is already written readable: the reconcile, squash and compact
//! endpoints, and the sweep that runs them across every aspect.
//!
//! These are the only endpoints that rewrite segments in place, so they are also
//! the only ones that can take the store's single writer lock: the response
//! carries whether a contended pass waited (`200` with `waited_millis`) or was
//! skipped (`409`, roadmap D-S7), never both.

use axum::{
	extract::{Path, State}, http::StatusCode, response::{IntoResponse, Response}, Json
};
use serde::{Deserialize, Serialize};
use weftdb::{MaintenanceBusy, MaintenanceWait};

use crate::{
	error::{ApiError, StrictQuery}, state::AppState
};

/// Classify a per-aspect maintenance error (reconcile, squash, compact): another
/// maintenance operation holding the aspect for longer than the store's maintenance wait
/// ([`MaintenanceBusy`]) is a `409`, to retry once it is done; an aspect whose name is not
/// safe as a file name, declared before names were checked, is a `400`, as on ingest and
/// read; anything else (a damaged frame, a filesystem or control-plane failure) is a
/// `500`.
fn maintenance_error(err: &anyhow::Error) -> ApiError {
	if err.downcast_ref::<MaintenanceBusy>().is_some() {
		ApiError::busy(format!("{err:#}"))
	} else if err.downcast_ref::<weftdb::InvalidAspectName>().is_some() {
		ApiError::bad_request(err.to_string())
	} else {
		ApiError::internal(err.to_string())
	}
}

/// Query parameters for `POST /api/v1/storage/{aspect}/reconcile`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReconcileParams {
	/// Optional order-health trigger threshold (roadmap Phase 4.6). When present,
	/// the pass runs **only** if the aspect's `unsorted_segments` backlog is at or
	/// above this many — the QuestDB-style split-count squash trigger, so a caller
	/// can poll cheaply and pay the rewrite only once the backlog is worth it. A
	/// value of 0 clamps to 1. When absent, the pass runs unconditionally (the
	/// original operator-trigger behaviour).
	pub threshold: Option<usize>,
	/// When `true`, reconcile in **hot/cold** mode (roadmap Phase 4.6): rewrite the
	/// aspect's cold (sealed, no-longer-appended) out-of-order segments regardless of
	/// the threshold, and defer only the hot tail (the most-recently-sealed segment)
	/// until the backlog reaches `threshold`. When absent/`false`, use the
	/// all-or-nothing threshold sweep. The response carries the cold/hot rewrite split.
	#[serde(default)]
	pub hot_cold: bool,
	/// When `true`, run the **cross-segment overlap merge** (roadmap Phase 4.6):
	/// collapse each group of time-overlapping segments into one (newer-wins),
	/// resolving late data that re-entered an already-covered window. Takes precedence
	/// over `hot_cold`/`threshold` (a distinct axis from intra-segment disorder);
	/// `reconciled` in the response is then the number of segments merged away. Only
	/// meaningful on the per-aspect endpoint.
	#[serde(default)]
	pub overlaps: bool,
	/// Split-not-rewrite floor in **bytes** for the overlap merge (roadmap Phase 4.6),
	/// paired with `overlaps=true`. When present, an overlap component whose cold prefix
	/// clears this many bytes *and* outweighs its hot suffix is **split** — the cold
	/// prefix carved into its own segment and only the hot suffix merged — instead of
	/// fully rewritten, so later late arrivals never rewrite the cold prefix again. When
	/// absent, the merge uses `SplitPolicy::questdb_default()` (a 50 MiB floor), under
	/// which small components always full-rewrite. Ignored unless `overlaps` is set.
	pub split_min_bytes: Option<u64>,
}

/// Response body for `POST /api/v1/storage/{aspect}/reconcile` — the outcome of an
/// out-of-order reconciliation pass.
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileResponse {
	/// The aspect reconciled.
	pub aspect: String,
	/// The sweep mode applied — `"threshold"` (all-or-nothing) or `"hot_cold"`.
	pub mode: &'static str,
	/// Whether the pass actually ran. Always `true` for an unconditional or hot/cold
	/// call; `false` when a `threshold` was given (threshold mode) and the backlog
	/// held below it (nothing read or rewritten).
	pub triggered: bool,
	/// Number of out-of-order segments rewritten sorted by this call (0 when the
	/// aspect was already fully ordered, or when the pass did not trigger).
	pub reconciled: usize,
	/// Cold (non-hot-tail) segments rewritten (hot/cold mode only; 0 in threshold mode).
	pub cold_reconciled: usize,
	/// Hot-tail segments rewritten (hot/cold mode only; 0 in threshold mode).
	pub hot_reconciled: usize,
	/// The intra-segment order-health count *after* the pass — zero once every segment
	/// admits ordered (binary-search) access.
	pub unsorted_segments: usize,
	/// The cross-segment overlap count *after* the pass — zero once no two segments'
	/// time windows intersect (the `overlaps` mode drives this to zero by merging).
	pub overlapping_segments: usize,
}

/// Handle `POST /api/v1/storage/{aspect}/reconcile`: rewrite every out-of-order
/// segment of `aspect` into a time-sorted one (roadmap Phase 4.6).
///
/// This is the operator trigger for the reconciliation pass the
/// `unsorted_segments` order-health signal motivates. With no `threshold` query
/// param it delegates to [`SegmentStore::reconcile_aspect`](weftdb::SegmentStore::reconcile_aspect)
/// unconditionally; with `?threshold=N` it delegates to the threshold-gated
/// [`reconcile_aspect_if_unsorted_exceeds`](weftdb::SegmentStore::reconcile_aspect_if_unsorted_exceeds),
/// so the rewrite runs only when the backlog is at or above `N`. Either way each
/// out-of-order segment is stable-sorted by timestamp and re-sealed in place at its
/// own id, so afterward point lookups over the aspect binary-search. A pass that
/// actually runs is counted in `weft_reconcile_passes_total` (holds below the
/// threshold are not). Returns `200 OK` with whether it triggered, the number
/// rewritten, and the post-pass order-health count.
///
/// Maintenance operations on one aspect take turns: while another one (a daemon pass,
/// another request) holds the aspect, this waits for it up to the store's maintenance
/// wait (30 s by default) and then answers `409 Conflict` without touching the aspect.
///
/// # Errors
///
/// [`ApiError::unconfigured`] when no store is attached,
/// [`ApiError::aspect_not_found`] when the aspect is undeclared,
/// [`ApiError::bad_request`] when its name is not safe as a file name (declared
/// before names were checked),
/// [`ApiError::busy`] when another maintenance operation held the aspect for the
/// whole wait, and [`ApiError::internal`] on a read/seal/control-plane failure.
pub async fn reconcile_aspect(State(state): State<AppState>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<ReconcileParams>) -> Result<Response, ApiError> {
	let store = state.store().cloned().ok_or(ApiError::unconfigured())?;
	let metrics = state.metrics().clone();
	drop(state);
	let result = reconcile_aspect_inner(&store, &aspect, params.threshold, params.hot_cold, params.overlaps, params.split_min_bytes, &metrics).await;
	drop(store);
	result
}

/// The body of [`reconcile_aspect`], split out so the significant-`Drop`
/// [`SegmentStore`](weftdb::SegmentStore) handle is dropped in the caller after
/// the last use rather than held across the response construction.
async fn reconcile_aspect_inner(store: &weftdb::SegmentStore, aspect: &str, threshold: Option<usize>, hot_cold: bool, overlaps: bool, split_min_bytes: Option<u64>, metrics: &crate::metrics::SharedMetrics) -> Result<Response, ApiError> {
	// Undeclared aspect → 404 (mirrors the read surface's not-found semantics).
	if store.schema_for(aspect).await.map_err(|err| ApiError::internal(err.to_string()))?.is_none() {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
	}
	// Three modes, in precedence order:
	// - overlaps: merge cross-segment time-overlap groups (a distinct axis from the
	//   intra-segment sort — `reconciled` is the number of segments merged away).
	// - hot/cold: always reconcile cold segments (the pass always runs), deferring the
	//   hot tail until the backlog reaches the threshold.
	// - threshold: all-or-nothing — `Some(threshold)` gates the whole pass, `None` runs
	//   it unconditionally; a gated call that holds is `false` (not a counted pass).
	let (mode, triggered, reconciled, cold_reconciled, hot_reconciled) = if overlaps {
		// `split_min_bytes` selects the split-not-rewrite floor; absent → the default
		// 50 MiB QuestDB floor (small components always full-rewrite).
		let removed = match split_min_bytes {
			Some(min) => store.reconcile_overlaps_with_policy(aspect, weft_physical_type::SplitPolicy::new(min)).await.map_err(|err| maintenance_error(&err))?,
			None => store.reconcile_overlaps(aspect).await.map_err(|err| maintenance_error(&err))?,
		};
		("overlaps", true, removed, 0, 0)
	} else if hot_cold {
		let outcome = store.reconcile_aspect_hot_cold(aspect, threshold.unwrap_or(1)).await.map_err(|err| maintenance_error(&err))?;
		("hot_cold", true, outcome.total(), outcome.cold_reconciled, outcome.hot_reconciled)
	} else {
		let (triggered, reconciled) = match threshold {
			Some(threshold) => store.reconcile_aspect_if_unsorted_exceeds(aspect, threshold).await.map_err(|err| maintenance_error(&err))?.map_or((false, 0), |reconciled| (true, reconciled)),
			None => (true, store.reconcile_aspect(aspect).await.map_err(|err| maintenance_error(&err))?),
		};
		("threshold", triggered, reconciled, 0, 0)
	};
	match (mode, triggered, reconciled) {
		// overlaps / hot/cold: record a pass only when it actually changed something
		// (matches the background daemon / store-wide sweep semantics).
		("threshold", true, n) => metrics.record_reconcile_pass(u64::try_from(n).unwrap_or(u64::MAX)),
		("threshold", false, _) => {}
		(_, _, n) if n > 0 => metrics.record_reconcile_pass(u64::try_from(n).unwrap_or(u64::MAX)),
		(_, _, _) => {}
	}
	let stats = store.aspect_stats(aspect).await.map_err(|err| ApiError::internal(err.to_string()))?;
	Ok((StatusCode::OK, Json(ReconcileResponse { aspect: aspect.to_string(), mode, triggered, reconciled, cold_reconciled, hot_reconciled, unsorted_segments: stats.unsorted_segments, overlapping_segments: stats.overlapping_segments })).into_response())
}

/// One aspect a store-wide sweep could not maintain, as listed in
/// [`ReconcileStoreResponse::failed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SweepFailure {
	/// The aspect whose pass failed.
	pub aspect: String,
	/// The failure with its full cause chain (for a damaged frame, the decode error and
	/// the frame's path).
	pub error: String,
}

impl SweepFailure {
	/// Render a sweep's `failed` list for the response body, keeping the whole cause
	/// chain (`{:#}`) so an operator sees *which* frame is damaged, not just that a
	/// decode failed.
	fn list(failed: &[(String, anyhow::Error)]) -> Vec<Self> {
		failed.iter().map(|(aspect, err)| Self { aspect: aspect.clone(), error: format!("{err:#}") }).collect()
	}
}

/// Response body for `POST /api/v1/storage/reconcile` — the outcome of a store-wide
/// threshold reconciliation sweep across every declared aspect.
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileStoreResponse {
	/// The order-health backlog threshold applied (an absent `?threshold=` defaults
	/// to 1 — reconcile any aspect with at least one out-of-order segment).
	pub threshold: usize,
	/// The sweep mode applied — `"threshold"` (all-or-nothing) or `"hot_cold"`.
	pub mode: &'static str,
	/// Number of declared aspects the sweep visited.
	pub aspects_scanned: usize,
	/// Number of aspects that were reconciled (met the threshold in threshold mode, or
	/// rewrote at least one cold/hot segment in hot/cold mode).
	pub aspects_reconciled: usize,
	/// Total out-of-order segments rewritten sorted across the sweep.
	pub segments_reconciled: usize,
	/// Total cold (non-hot-tail) segments rewritten (hot/cold mode only; 0 in threshold mode).
	pub cold_reconciled: usize,
	/// Total hot-tail segments rewritten (hot/cold mode only; 0 in threshold mode).
	pub hot_reconciled: usize,
	/// The store-wide order-health count *after* the sweep — zero once every segment
	/// in the store admits ordered (binary-search) access.
	pub unsorted_segments: usize,
	/// The store-wide cross-segment overlap count *after* the sweep — zero once no two
	/// segments' windows intersect (the `overlaps` mode drives this to zero).
	pub overlapping_segments: usize,
	/// The aspects whose pass failed, each `{aspect, error}`, in the order the sweep
	/// visited them (declared-name order). Empty when every aspect succeeded.
	///
	/// **Response change (crash-consistency S4):** a per-aspect failure used to fail the
	/// whole request with a `500` at the first failing aspect, leaving every aspect after
	/// it in name order unswept. The sweep now visits every aspect and answers `200` with
	/// the failures listed here, so a caller must check this list (or
	/// `weft_reconcile_failed_passes_total`) to tell a partial sweep from a clean one. A
	/// failed aspect is counted in `aspects_scanned` only. Failing to list the aspects at
	/// all is still a `500`.
	pub failed: Vec<SweepFailure>,
}

/// Handle `POST /api/v1/storage/reconcile`: sweep **every** declared aspect,
/// reconciling those whose out-of-order backlog is at or above `?threshold=N`
/// (roadmap Phase 4.6).
///
/// The manual, store-wide operator counterpart to the per-aspect
/// [`reconcile_aspect`] endpoint and the background reconcile daemon — the same
/// [`SegmentStore::reconcile_all_over_threshold`](weftdb::SegmentStore::reconcile_all_over_threshold)
/// sweep, on demand. An absent `threshold` defaults to 1 (reconcile any aspect with
/// out-of-order data). Each aspect actually reconciled is counted in
/// `weft_reconcile_passes_total`, exactly like the per-aspect trigger and the daemon.
///
/// With `?hot_cold=true` it instead runs the
/// [`SegmentStore::reconcile_all_hot_cold`](weftdb::SegmentStore::reconcile_all_hot_cold)
/// sweep: every aspect's cold segments are reconciled unconditionally and only the
/// hot tail is gated on the threshold. The response then carries the cold/hot split.
///
/// With `?overlaps=true` it runs the store-wide cross-segment overlap merge
/// ([`SegmentStore::reconcile_all_overlaps`](weftdb::SegmentStore::reconcile_all_overlaps)),
/// merging each aspect's time-overlap groups; `segments_reconciled` is then the number
/// of segments merged away. `overlaps` takes precedence over `hot_cold`/`threshold`.
///
/// An aspect whose pass fails (a damaged frame, an I/O error) does not stop the sweep:
/// it is listed in [`ReconcileStoreResponse::failed`], counted in
/// `weft_reconcile_failed_passes_total`, and the remaining aspects are still swept; the
/// response is a `200` either way.
///
/// An aspect another maintenance operation holds (a daemon pass, another request) is
/// waited for, with one maintenance wait (the store's, 30 s by default) shared by the
/// whole sweep. If an aspect is still held when that runs out, the sweep leaves it alone,
/// maintains the rest, and answers `409 Conflict` naming the aspects it left, so the
/// caller knows to retry.
///
/// # Errors
///
/// [`ApiError::unconfigured`] when no store is attached,
/// [`ApiError::busy`] when other maintenance operations held some aspects for
/// the whole wait, and [`ApiError::internal`] when the aspect list or the post-sweep
/// store stats cannot be read.
pub async fn reconcile_store(State(state): State<AppState>, StrictQuery(params): StrictQuery<ReconcileParams>) -> Result<Response, ApiError> {
	let store = state.store().cloned().ok_or(ApiError::unconfigured())?;
	let metrics = state.metrics().clone();
	drop(state);
	let result = reconcile_store_inner(&store, params.threshold, params.hot_cold, params.overlaps, params.split_min_bytes, &metrics).await;
	drop(store);
	result
}

/// The body of [`reconcile_store`], split out so the significant-`Drop`
/// [`SegmentStore`](weftdb::SegmentStore) handle is dropped in the caller.
async fn reconcile_store_inner(store: &weftdb::SegmentStore, threshold: Option<usize>, hot_cold: bool, overlaps: bool, split_min_bytes: Option<u64>, metrics: &crate::metrics::SharedMetrics) -> Result<Response, ApiError> {
	let threshold = threshold.unwrap_or(1).max(1);
	// Wait out a daemon pass (or another request) on an aspect rather than skip it.
	let wait = MaintenanceWait::Wait(store.maintenance_wait());
	let (mode, aspects_scanned, aspects_reconciled, segments_reconciled, cold_reconciled, hot_reconciled, failed, busy) = if overlaps {
		// `split_min_bytes` selects the store-wide split-not-rewrite floor; absent → the
		// default 50 MiB floor (every aspect's small components full-rewrite).
		let sweep = match split_min_bytes {
			Some(min) => store.reconcile_all_overlaps_with_policy(weft_physical_type::SplitPolicy::new(min), wait).await.map_err(|err| ApiError::internal(err.to_string()))?,
			None => store.reconcile_all_overlaps(wait).await.map_err(|err| ApiError::internal(err.to_string()))?,
		};
		if sweep.aspects_reconciled > 0 {
			metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_removed).unwrap_or(u64::MAX));
		}
		("overlaps", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_removed, 0, 0, SweepFailure::list(&sweep.failed), sweep.busy)
	} else if hot_cold {
		let sweep = store.reconcile_all_hot_cold(threshold, wait).await.map_err(|err| ApiError::internal(err.to_string()))?;
		if sweep.aspects_reconciled > 0 {
			metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_reconciled()).unwrap_or(u64::MAX));
		}
		("hot_cold", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_reconciled(), sweep.cold_reconciled, sweep.hot_reconciled, SweepFailure::list(&sweep.failed), sweep.busy)
	} else {
		let sweep = store.reconcile_all_over_threshold(threshold, wait).await.map_err(|err| ApiError::internal(err.to_string()))?;
		if sweep.aspects_reconciled > 0 {
			metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_reconciled).unwrap_or(u64::MAX));
		}
		("threshold", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_reconciled, 0, 0, SweepFailure::list(&sweep.failed), sweep.busy)
	};
	if !failed.is_empty() {
		metrics.record_reconcile_failures(u64::try_from(failed.len()).unwrap_or(u64::MAX));
	}
	if !busy.is_empty() {
		return Err(ApiError::busy(format!("aspects {busy:?} were held by other maintenance operations for the whole {:?} wait and were not swept; the other {} aspects were swept; retry", store.maintenance_wait(), aspects_scanned.saturating_sub(busy.len()))));
	}
	let unsorted_segments = store.store_stats().await.map_err(|err| ApiError::internal(err.to_string()))?.unsorted_segments;
	let overlapping_segments = store.store_overlapping_segments().await.map_err(|err| ApiError::internal(err.to_string()))?;
	Ok((StatusCode::OK, Json(ReconcileStoreResponse { threshold, mode, aspects_scanned, aspects_reconciled, segments_reconciled, cold_reconciled, hot_reconciled, unsorted_segments, overlapping_segments, failed })).into_response())
}

/// Query parameters for `POST /api/v1/storage/{aspect}/squash`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SquashParams {
	/// Optional segment-count cap (roadmap Phase 4.6): squash **only** when the aspect
	/// has more than this many segments — the `QuestDB`-`max.splits`-style trigger that
	/// bounds split-path fragmentation. A value of 0 clamps to 1. When absent, the
	/// squash runs unconditionally.
	pub max_segments: Option<usize>,
}

/// Response body for `POST /api/v1/storage/{aspect}/squash` — the outcome of a
/// segment squash pass.
#[derive(Debug, Clone, Serialize)]
pub struct SquashResponse {
	/// The aspect squashed.
	pub aspect: String,
	/// Whether the squash actually ran. `false` only when a `max_segments` cap was
	/// given and the aspect's segment count held at or below it (nothing rewritten).
	pub triggered: bool,
	/// Number of segments removed by folding the aspect into one (0 when it already had
	/// fewer than two segments, or the pass did not trigger).
	pub removed: usize,
	/// The aspect's segment count *after* the pass — 1 once a squash has folded it.
	pub segment_count: usize,
}

/// Handle `POST /api/v1/storage/{aspect}/squash`: fold an aspect's segments into one
/// (roadmap Phase 4.6 — the squash half of the split-not-rewrite path).
///
/// With no `max_segments` query param it squashes unconditionally
/// ([`SegmentStore::squash_aspect`](weftdb::SegmentStore::squash_aspect)); with
/// `?max_segments=N` it delegates to the threshold-gated
/// [`squash_aspect_if_exceeds`](weftdb::SegmentStore::squash_aspect_if_exceeds), so
/// the rewrite runs only when the segment count exceeds `N` — the trigger that bounds
/// the fragmentation repeated split carve-offs create. Returns `200 OK` with whether
/// it triggered, how many segments it removed, and the post-pass segment count. Like
/// the reconcile endpoint, it waits for an aspect another maintenance operation holds up
/// to the store's maintenance wait (30 s by default), then answers `409 Conflict`.
///
/// # Errors
///
/// [`ApiError::unconfigured`] when no store is attached,
/// [`ApiError::aspect_not_found`] when the aspect is undeclared,
/// [`ApiError::bad_request`] when its name is not safe as a file name (declared
/// before names were checked),
/// [`ApiError::busy`] when another maintenance operation held the aspect for the
/// whole wait, and [`ApiError::internal`] on a read/seal/control-plane failure.
pub async fn squash_aspect(State(state): State<AppState>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<SquashParams>) -> Result<Response, ApiError> {
	let store = state.store().cloned().ok_or(ApiError::unconfigured())?;
	drop(state);
	// Undeclared aspect → 404 (mirrors the reconcile/read surfaces).
	if store.schema_for(&aspect).await.map_err(|err| ApiError::internal(err.to_string()))?.is_none() {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
	}
	let (triggered, removed) = match params.max_segments {
		Some(max) => store.squash_aspect_if_exceeds(&aspect, max).await.map_err(|err| maintenance_error(&err))?.map_or((false, 0), |removed| (true, removed)),
		None => (true, store.squash_aspect(&aspect).await.map_err(|err| maintenance_error(&err))?),
	};
	let segment_count = store.segment_count(&aspect).await.map_err(|err| ApiError::internal(err.to_string()))?;
	drop(store);
	Ok((StatusCode::OK, Json(SquashResponse { aspect, triggered, removed, segment_count })).into_response())
}

/// Query parameters for `POST /api/v1/storage/{aspect}/compact`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompactParams {
	/// The target segment size in **rows** (required). The aspect's segments are
	/// coalesced toward ~this many rows per segment, leaving already-large segments
	/// untouched — holding fragmentation near the read-optimal size rather than folding
	/// to one (which `squash` does). Absent → `400`.
	pub target_rows: Option<usize>,
}

/// Response body for `POST /api/v1/storage/{aspect}/compact` — the outcome of a
/// size-targeted compaction pass.
#[derive(Debug, Clone, Serialize)]
pub struct CompactResponse {
	/// The aspect compacted.
	pub aspect: String,
	/// Number of segments removed by coalescing groups toward the target size (0 when
	/// nothing coalesced — fewer than two segments, or every segment already ≥ target).
	pub removed: usize,
	/// The aspect's segment count *after* the pass.
	pub segment_count: usize,
}

/// Handle `POST /api/v1/storage/{aspect}/compact?target_rows=N`: coalesce an aspect's
/// segments toward ~`N` rows per segment (roadmap Phase 4.6 — the size-aware
/// counterpart of `squash`, which folds to one).
///
/// The manual counterpart of the `WEFT_COMPACT_TARGET_ROWS` daemon pass, delegating to
/// [`SegmentStore::squash_aspect_to_target_rows`](weftdb::SegmentStore::squash_aspect_to_target_rows).
/// Returns `200 OK` with how many segments it removed and the post-pass segment count.
/// Like the reconcile endpoint, it waits for an aspect another maintenance operation holds
/// up to the store's maintenance wait (30 s by default), then answers `409 Conflict`.
///
/// # Errors
///
/// [`ApiError::unconfigured`] when no store is attached,
/// [`ApiError::bad_request`] when `target_rows` is absent or the aspect's name is not
/// safe as a file name (declared before names were checked),
/// [`ApiError::aspect_not_found`] when the aspect is undeclared,
/// [`ApiError::busy`] when another maintenance operation held the aspect for the
/// whole wait, and [`ApiError::internal`] on a read/seal/control-plane failure.
pub async fn compact_aspect(State(state): State<AppState>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<CompactParams>) -> Result<Response, ApiError> {
	let store = state.store().cloned().ok_or(ApiError::unconfigured())?;
	drop(state);
	let target_rows = params.target_rows.ok_or_else(|| ApiError::bad_request("compact requires a target_rows query parameter".to_string()))?;
	// Undeclared aspect → 404 (mirrors the reconcile/squash surfaces).
	if store.schema_for(&aspect).await.map_err(|err| ApiError::internal(err.to_string()))?.is_none() {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
	}
	let removed = store.squash_aspect_to_target_rows(&aspect, target_rows).await.map_err(|err| maintenance_error(&err))?;
	let segment_count = store.segment_count(&aspect).await.map_err(|err| ApiError::internal(err.to_string()))?;
	drop(store);
	Ok((StatusCode::OK, Json(CompactResponse { aspect, removed, segment_count })).into_response())
}

#[cfg(test)]

mod tests {

	use std::sync::Arc;

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tempfile::TempDir;
	use tower::ServiceExt;
	use weftdb::SegmentStore;

	use crate::{app_with_state, test_support::*, AppState};

	#[tokio::test]
	async fn reconcile_endpoint_orders_an_out_of_order_aspect() {
		let dir = TempDir::new().unwrap();
		// Ingest an out-of-order batch (no require_sorted) — it seals unsorted.
		let router = router_with_declared_price(&dir).await;
		let ooo = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 130, "value": "4.5" },
			{ "timestamp": 110, "value": "2.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &ooo).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["time_sorted"], false, "an out-of-order batch seals unsorted");

		// Before reconciliation the aspect reports one out-of-order segment.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/stats").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let before: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(before["unsorted_segments"], 1);

		// Reconcile via the endpoint: one segment rewritten, order-health now clean.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/reconcile").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["aspect"], "price");
		assert_eq!(json["reconciled"], 1);
		assert_eq!(json["unsorted_segments"], 0);

		// The rows read back in timestamp order after reconciliation.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/points?start=0&end=1000").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let read: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		let timestamps: Vec<i64> = read["points"].as_array().unwrap().iter().map(|p| p["timestamp"].as_i64().unwrap()).collect();
		assert_eq!(timestamps, vec![100, 110, 130]);
	}

	#[tokio::test]
	async fn reconcile_endpoint_threshold_gates_the_pass() {
		let dir = TempDir::new().unwrap();
		// Ingest one out-of-order batch — the backlog is a single unsorted segment.
		let router = router_with_declared_price(&dir).await;
		let ooo = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 130, "value": "4.5" },
			{ "timestamp": 110, "value": "2.5" },
		] });
		let (status, _body) = post_json(router, "/api/v1/storage/price/points", &ooo).await;
		assert_eq!(status, StatusCode::CREATED);

		// threshold=2 is not met by a backlog of 1: the pass holds, nothing rewritten.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/reconcile?threshold=2").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let held: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(held["triggered"], false, "below the threshold the pass does not run");
		assert_eq!(held["reconciled"], 0);
		assert_eq!(held["unsorted_segments"], 1, "the backlog is left out of order");

		// threshold=1 is met: the pass fires and clears the backlog.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/reconcile?threshold=1").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let fired: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(fired["triggered"], true, "at the threshold the pass runs");
		assert_eq!(fired["reconciled"], 1);
		assert_eq!(fired["unsorted_segments"], 0);
	}

	#[tokio::test]
	async fn reconcile_store_endpoint_sweeps_every_aspect() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		store.declare("b", &sc).await.expect("declares b");
		// One out-of-order segment in each aspect.
		store.seal("a", &sc, &[100_i64, 130, 110], &["1".parse().unwrap(), "3".parse().unwrap(), "2".parse().unwrap()]).await.expect("seal a");
		store.seal("b", &sc, &[200_i64, 240, 210], &["4".parse().unwrap(), "6".parse().unwrap(), "5".parse().unwrap()]).await.expect("seal b");

		// Store-wide sweep with no threshold (defaults to 1): both aspects reconciled.
		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/reconcile").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["threshold"], 1);
		assert_eq!(json["aspects_scanned"], 2);
		assert_eq!(json["aspects_reconciled"], 2);
		assert_eq!(json["segments_reconciled"], 2);
		assert_eq!(json["unsorted_segments"], 0);
		assert_eq!(json["overlapping_segments"], 0, "the reconciled segments cover disjoint windows");
		assert_eq!(json["failed"], serde_json::json!([]), "a clean sweep lists no failures");
	}

	/// Crash-consistency S4: six aspects (`s1`..`s6`), each with three out-of-order,
	/// time-overlapping segments, and `s2`'s lowest-id frame truncated to half its length.
	/// Every store-wide reconcile mode must answer `200`, sweep the five healthy aspects and
	/// list `s2` (with the damaged frame in its error) in `failed`; before S4 the request
	/// failed with a `500` at `s2` and `s3`..`s6` were never swept.
	#[tokio::test]
	async fn reconcile_store_endpoint_lists_a_failed_aspect_and_sweeps_the_rest() {
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let v = |s: &str| -> bigdecimal::BigDecimal { s.parse().unwrap() };
		for uri in ["/api/v1/storage/reconcile", "/api/v1/storage/reconcile?hot_cold=true", "/api/v1/storage/reconcile?overlaps=true", "/api/v1/storage/reconcile?overlaps=true&split_min_bytes=1"] {
			let dir = TempDir::new().unwrap();
			let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
			for aspect in ["s1", "s2", "s3", "s4", "s5", "s6"] {
				store.declare(aspect, &sc).await.expect("declares");
				let first = store.seal(aspect, &sc, &[100_i64, 130, 110], &[v("1"), v("3"), v("2")]).await.expect("seal 0");
				store.seal(aspect, &sc, &[120_i64, 150, 125], &[v("4"), v("6"), v("5")]).await.expect("seal 1");
				store.seal(aspect, &sc, &[140_i64, 170, 145], &[v("7"), v("9"), v("8")]).await.expect("seal 2");
				if aspect == "s2" {
					let file = std::fs::OpenOptions::new().write(true).open(&first.path).expect("opens frame");
					let len = file.metadata().expect("stats frame").len();
					file.set_len(len / 2).expect("truncates frame");
				}
			}
			let router = app_with_state(AppState::new().with_store(store.clone()));
			let response = router.oneshot(Request::builder().method("POST").uri(uri).body(Body::empty()).unwrap()).await.unwrap();
			let status = response.status();
			let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
			let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
			let s6 = store.aspect_stats("s6").await.unwrap();
			drop(store);
			assert_eq!(status, StatusCode::OK, "{uri}: a failed aspect does not fail the request: {json}");
			assert_eq!(json["aspects_scanned"], 6, "{uri}");
			assert_eq!(json["aspects_reconciled"], 5, "{uri}: every healthy aspect is swept: {json}");
			let failed = json["failed"].as_array().unwrap_or_else(|| panic!("{uri}: failed is a list: {json}"));
			assert_eq!(failed.len(), 1, "{uri}: only the damaged aspect fails: {json}");
			assert_eq!(failed[0]["aspect"], "s2", "{uri}");
			assert!(failed[0]["error"].as_str().unwrap_or_default().contains("s2-0.weftseg"), "{uri}: the error names the damaged frame: {json}");
			let reached = if uri.contains("overlaps") { s6.overlapping_segments == 0 } else { s6.unsorted_segments == 0 };
			assert!(reached, "{uri}: the last aspect in name order is still reached: {s6:?}");
		}
	}

	#[tokio::test]
	async fn compact_endpoint_coalesces_toward_target_and_requires_the_param() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("load", &sc).await.expect("declares");
		// Six disjoint 2-row segments; target 6 coalesces them in triples → two segments.
		for seg in 0..6_i64 {
			let base = seg * 100;
			store.seal("load", &sc, &[base, base + 10], &["1".parse().unwrap(), "2".parse().unwrap()]).await.expect("seals");
		}

		// Missing target_rows → 400.
		let router = app_with_state(AppState::new().with_store(store.clone()));
		let bad = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/load/compact").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "compact needs a target_rows param");

		// target_rows=6 coalesces six 2-row segments → two 6-row segments (4 removed).
		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/load/compact?target_rows=6").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["aspect"], "load");
		assert_eq!(json["removed"], 4, "six segments coalesced to two (two triples)");
		assert_eq!(json["segment_count"], 2, "held near the target size, not folded to one");
	}

	#[tokio::test]
	async fn reconcile_store_endpoint_overlaps_merges_across_aspects() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		store.declare("b", &sc).await.expect("declares b");
		// a: an overlapping pair. b: disjoint segments (untouched).
		store.seal("a", &sc, &[0_i64, 10, 20], &["1".parse().unwrap(), "2".parse().unwrap(), "3".parse().unwrap()]).await.expect("a older");
		store.seal("a", &sc, &[10_i64, 20, 30], &["4".parse().unwrap(), "5".parse().unwrap(), "6".parse().unwrap()]).await.expect("a newer");
		store.seal("b", &sc, &[0_i64, 10, 20], &["7".parse().unwrap(), "8".parse().unwrap(), "9".parse().unwrap()]).await.expect("b lo");
		store.seal("b", &sc, &[100_i64, 110, 120], &["1".parse().unwrap(), "2".parse().unwrap(), "3".parse().unwrap()]).await.expect("b hi");

		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/reconcile?overlaps=true").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["mode"], "overlaps");
		assert_eq!(json["aspects_reconciled"], 1, "only a had an overlap to merge");
		assert_eq!(json["segments_reconciled"], 1, "one segment merged away");
		assert_eq!(json["overlapping_segments"], 0, "no cross-segment overlap remains store-wide");
	}

	#[tokio::test]
	async fn reconcile_store_endpoint_split_min_bytes_carves_cold_prefixes() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		// A dominant cold base + a small late tail that re-enters it.
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_vs: Vec<bigdecimal::BigDecimal> = (0..=10).map(|i| i.to_string().parse().unwrap()).collect();
		store.seal("a", &sc, &base_ts, &base_vs).await.expect("base");
		store.seal("a", &sc, &[90_i64, 100, 110], &["900".parse().unwrap(), "1000".parse().unwrap(), "1100".parse().unwrap()]).await.expect("late");

		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/reconcile?overlaps=true&split_min_bytes=1").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		let segments_after = store.segment_count("a").await.unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["aspects_reconciled"], 1, "a carried overlap and was split");
		assert_eq!(json["overlapping_segments"], 0);
		assert_eq!(segments_after, 2, "the store-wide split carved the cold prefix off (vs 1 under the default floor)");
	}

	#[tokio::test]
	async fn reconcile_store_endpoint_hot_cold_defers_hot_tails() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		store.declare("b", &sc).await.expect("declares b");
		// aspect a: a cold + a hot out-of-order segment (backlog 2). aspect b: a lone
		// out-of-order segment, which is itself the hot tail (backlog 1).
		store.seal("a", &sc, &[100_i64, 130, 110], &["1".parse().unwrap(), "3".parse().unwrap(), "2".parse().unwrap()]).await.expect("a cold");
		store.seal("a", &sc, &[200_i64, 240, 210], &["4".parse().unwrap(), "6".parse().unwrap(), "5".parse().unwrap()]).await.expect("a hot");
		store.seal("b", &sc, &[100_i64, 130, 110], &["7".parse().unwrap(), "9".parse().unwrap(), "8".parse().unwrap()]).await.expect("b hot only");

		// Hot/cold sweep at threshold 2: a's cold segment reconciles, a's hot tail fires
		// (backlog 2), b's lone hot tail is deferred (backlog 1 < 2).
		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/reconcile?hot_cold=true&threshold=2").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		let b_after = store.aspect_stats("b").await.unwrap().unsorted_segments;
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["mode"], "hot_cold");
		assert_eq!(json["threshold"], 2);
		assert_eq!(json["aspects_scanned"], 2);
		assert_eq!(json["aspects_reconciled"], 1, "only a rewrote a segment");
		assert_eq!(json["cold_reconciled"], 1, "a's cold segment");
		assert_eq!(json["hot_reconciled"], 1, "a's hot tail at threshold 2");
		assert_eq!(json["segments_reconciled"], 2);
		assert_eq!(json["unsorted_segments"], 1, "b's deferred hot tail remains out of order");
		assert_eq!(b_after, 1);
	}

	#[tokio::test]
	async fn reconcile_aspect_endpoint_hot_cold_reconciles_cold_defers_hot() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		store.seal("a", &sc, &[100_i64, 130, 110], &["1".parse().unwrap(), "3".parse().unwrap(), "2".parse().unwrap()]).await.expect("a cold");
		store.seal("a", &sc, &[200_i64, 240, 210], &["4".parse().unwrap(), "6".parse().unwrap(), "5".parse().unwrap()]).await.expect("a hot");

		// Per-aspect hot/cold at threshold 3 (above the backlog of 2): cold reconciled,
		// hot tail deferred.
		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/a/reconcile?hot_cold=true&threshold=3").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["mode"], "hot_cold");
		assert_eq!(json["triggered"], true, "a hot/cold pass always runs the cold reconciliation");
		assert_eq!(json["cold_reconciled"], 1);
		assert_eq!(json["hot_reconciled"], 0, "the hot tail is deferred below threshold 3");
		assert_eq!(json["reconciled"], 1);
		assert_eq!(json["unsorted_segments"], 1, "the hot tail remains out of order");
	}

	#[tokio::test]
	async fn reconcile_aspect_endpoint_overlaps_merges_overlapping_segments() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		// Two internally-sorted but time-overlapping segments ([0,20] and [10,30]).
		store.seal("a", &sc, &[0_i64, 10, 20], &["1".parse().unwrap(), "2".parse().unwrap(), "3".parse().unwrap()]).await.expect("older");
		store.seal("a", &sc, &[10_i64, 20, 30], &["4".parse().unwrap(), "5".parse().unwrap(), "6".parse().unwrap()]).await.expect("newer");

		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/a/reconcile?overlaps=true").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		let segments_after = store.segment_count("a").await.unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["mode"], "overlaps");
		assert_eq!(json["reconciled"], 1, "one segment merged away");
		assert_eq!(json["overlapping_segments"], 0, "no cross-segment overlap remains");
		assert_eq!(segments_after, 1, "the overlapping pair collapsed to one segment");
	}

	#[tokio::test]
	async fn reconcile_aspect_endpoint_split_min_bytes_carves_the_cold_prefix() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		// A long cold base [0..100] plus a small late tail [90,100,110] that re-enters it.
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_vs: Vec<bigdecimal::BigDecimal> = (0..=10).map(|i| i.to_string().parse().unwrap()).collect();
		store.seal("a", &sc, &base_ts, &base_vs).await.expect("base");
		store.seal("a", &sc, &[90_i64, 100, 110], &["900".parse().unwrap(), "1000".parse().unwrap(), "1100".parse().unwrap()]).await.expect("late");

		let router = app_with_state(AppState::new().with_store(store.clone()));
		// A tiny split floor forces the dominant cold prefix to split off.
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/a/reconcile?overlaps=true&split_min_bytes=1").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		let segments_after = store.segment_count("a").await.unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["mode"], "overlaps");
		assert_eq!(json["overlapping_segments"], 0, "the two halves are disjoint");
		assert_eq!(segments_after, 2, "cold prefix split off from the hot suffix (vs 1 under the default floor)");
	}

	#[tokio::test]
	async fn squash_endpoint_folds_segments_into_one() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		// Three time-disjoint segments (as repeated split carve-offs leave behind).
		store.seal("a", &sc, &[0_i64, 10], &["0".parse().unwrap(), "1".parse().unwrap()]).await.expect("s0");
		store.seal("a", &sc, &[20_i64, 30], &["2".parse().unwrap(), "3".parse().unwrap()]).await.expect("s1");
		store.seal("a", &sc, &[40_i64, 50], &["4".parse().unwrap(), "5".parse().unwrap()]).await.expect("s2");

		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/a/squash").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		let count_after = store.segment_count("a").await.unwrap();
		drop(store);
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["triggered"], true);
		assert_eq!(json["removed"], 2, "three segments folded to one");
		assert_eq!(json["segment_count"], 1);
		assert_eq!(count_after, 1);
	}

	#[tokio::test]
	async fn squash_endpoint_max_segments_gates_the_pass() {
		let dir = TempDir::new().unwrap();
		let sc = weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("a", &sc).await.expect("declares a");
		store.seal("a", &sc, &[0_i64, 10], &["0".parse().unwrap(), "1".parse().unwrap()]).await.expect("s0");
		store.seal("a", &sc, &[20_i64, 30], &["2".parse().unwrap(), "3".parse().unwrap()]).await.expect("s1");

		// Cap 5 → holds (2 segments not > 5).
		let router = app_with_state(AppState::new().with_store(store.clone()));
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/a/squash?max_segments=5").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(json["triggered"], false, "held below the cap");
		assert_eq!(json["removed"], 0);
		assert_eq!(json["segment_count"], 2, "nothing rewritten");
		drop(store);
	}

	#[tokio::test]
	async fn squash_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/never_declared/squash").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn reconcile_store_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/reconcile").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn reconcile_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/never_declared/reconcile").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn reconcile_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/reconcile").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}
}
