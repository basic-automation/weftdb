//! The catalog-**management** endpoints (roadmap Phase 2 — *create DB / subject /
//! aspect; define schema/physical type; batch ingest*).
//!
//! These are the write-path counterpart of the read-only
//! [`storage`](crate::storage) surface. Until now the segment store could only be
//! populated **in-process** (a test or an
//! embedding host called [`SegmentStore::declare`](database::SegmentStore::declare)
//! / `seal_declared` directly); the read endpoints served whatever was already on
//! disk. These handlers let an HTTP client declare an aspect's schema and ingest a
//! batch of points, so a client can populate the store the
//! [`storage`](crate::storage) read endpoints then serve — closing the Phase-2
//! "DB/subject/aspect management" + "schema & physical-type definition" + "batch
//! ingest" gaps.
//!
//! ## Scope
//!
//! A configured [`SegmentStore`](database::SegmentStore) is opened against one
//! `(database, subject)` namespace (the binary's `DSP_SEGMENT_STORE_ROOT` wiring),
//! so these endpoints manage **aspects** within that scope — the unit a client
//! actually declares a schema for and seals batches into. The DB/subject hierarchy
//! is fixed at server start; aspect declaration + ingest is the live write path.
//!
//! ## Vendor-neutrality / storage boundary
//!
//! As with [`storage`](crate::storage) the heavy `arrow-*` tree never appears here:
//! these are plain JSON request/response handlers over the `database` control plane
//! and the typed `.dspseg` seal path. The declared `value_tolerance` is honoured by
//! the seal (hard constraint #4 — an ingest whose values cannot be represented
//! under the declared encoding within tolerance is **rejected**, never silently
//! downcast).

use axum::{
	extract::{Path, Query, State}, http::StatusCode, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use database::VerifyMode;
use dsp_line_protocol::TimestampPrecision;
use dsp_physical_type::{first_order_violation, AspectSchema, PhysicalType, SegmentDescriptor, TimeUnit};
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use crate::{state::AppState, storage::{AspectInfo, StorageError}};

/// Parse a [`PhysicalType`] from its stable wire token (the inverse of
/// [`PhysicalType::name`]) plus an optional `scale`.
///
/// The fixed-point encodings (`scaled_i64`, `scaled_i128`) require a `scale` (the
/// number of fractional decimal digits the mantissa carries); the others ignore it.
///
/// # Errors
///
/// Returns a human-readable message when the token is unknown, or when a
/// scale-bearing encoding was named without a `scale`.
fn parse_physical_type(token: &str, scale: Option<u8>) -> Result<PhysicalType, String> {
	match token {
		"f64" => Ok(PhysicalType::F64),
		"f32" => Ok(PhysicalType::F32),
		"scaled_i64" => scale.map(|scale| PhysicalType::ScaledI64 { scale }).ok_or_else(|| "`scaled_i64` requires a `scale` (fractional decimal digits)".to_string()),
		"scaled_i128" => scale.map(|scale| PhysicalType::ScaledI128 { scale }).ok_or_else(|| "`scaled_i128` requires a `scale` (fractional decimal digits)".to_string()),
		"decimal128" => Ok(PhysicalType::Decimal128),
		"bigdecimal_text" => Ok(PhysicalType::BigDecimalText),
		other => Err(format!("unknown physical_type {other:?} (expected one of f64, f32, scaled_i64, scaled_i128, decimal128, bigdecimal_text)")),
	}
}

/// Parse a [`TimeUnit`] from its stable wire token (the inverse of
/// [`TimeUnit::name`]).
///
/// # Errors
///
/// Returns a human-readable message when the token is unknown.
fn parse_time_unit(token: &str) -> Result<TimeUnit, String> {
	match token {
		"seconds" => Ok(TimeUnit::Seconds),
		"millis" => Ok(TimeUnit::Millis),
		"micros" => Ok(TimeUnit::Micros),
		"nanos" => Ok(TimeUnit::Nanos),
		other => Err(format!("unknown timestamp_unit {other:?} (expected one of seconds, millis, micros, nanos)")),
	}
}

/// Request body for `POST /api/v1/storage/aspects` — declare an aspect's schema.
///
/// The `physical_type`/`timestamp_unit` tokens mirror the read surface
/// ([`AspectInfo`]); `scale` is required only for the fixed-point encodings, and
/// `value_tolerance` defaults to `"0"` (exact) when omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct DeclareAspectRequest {
	/// The aspect name to declare (unique within the store's `(database, subject)`
	/// scope; a re-declaration overwrites).
	pub name: String,
	/// The physical encoding token (e.g. `"f64"`, `"scaled_i64"`).
	pub physical_type: String,
	/// Fractional-decimal-digit scale, required for `scaled_i64` / `scaled_i128`.
	#[serde(default)]
	pub scale: Option<u8>,
	/// The permitted per-value reconstruction error as decimal text. Defaults to
	/// `"0"` (the encoding must be exact for every value).
	#[serde(default)]
	pub value_tolerance: Option<String>,
	/// The timestamp resolution token (e.g. `"seconds"`, `"millis"`).
	pub timestamp_unit: String,
}

/// Response body for a successful aspect declaration: the schema the store now
/// holds, echoed in the same shape the read surface lists it ([`AspectInfo`]).
#[derive(Debug, Clone, Serialize)]
pub struct DeclareAspectResponse {
	/// The declared aspect.
	pub aspect: AspectInfo,
}

/// Build the [`AspectSchema`] a [`DeclareAspectRequest`] describes, mapping every
/// parse failure to a `400`.
fn schema_from_request(request: &DeclareAspectRequest) -> Result<AspectSchema, StorageError> {
	let physical_type = parse_physical_type(&request.physical_type, request.scale).map_err(StorageError::BadRequest)?;
	let timestamp_unit = parse_time_unit(&request.timestamp_unit).map_err(StorageError::BadRequest)?;
	let tolerance_text = request.value_tolerance.as_deref().unwrap_or("0");
	let value_tolerance: BigDecimal = tolerance_text.parse().map_err(|_| StorageError::BadRequest(format!("`value_tolerance` is not a decimal: {tolerance_text:?}")))?;
	Ok(AspectSchema::new(physical_type, value_tolerance, timestamp_unit))
}

/// Handle `POST /api/v1/storage/aspects`: declare an aspect's schema (physical
/// encoding + value tolerance + timestamp unit) in the configured store.
///
/// Returns `201 Created` with the stored schema. A re-declaration of the same
/// aspect overwrites and still returns `201`.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::BadRequest`] when a token/tolerance does not parse, and
/// [`StorageError::Internal`] on a control-plane write failure.
pub async fn declare_aspect(State(state): State<AppState>, Json(request): Json<DeclareAspectRequest>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let schema = schema_from_request(&request)?;
	let result = store.declare(&request.name, &schema).await;
	drop(store);
	result.map_err(|err| StorageError::Internal(err.to_string()))?;
	let aspect = AspectInfo { name: request.name, physical_type: schema.value.name(), value_tolerance: schema.value_tolerance.to_string(), timestamp_unit: schema.timestamp_unit.name() };
	Ok((StatusCode::CREATED, Json(DeclareAspectResponse { aspect })).into_response())
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
/// param it delegates to [`SegmentStore::reconcile_aspect`](database::SegmentStore::reconcile_aspect)
/// unconditionally; with `?threshold=N` it delegates to the threshold-gated
/// [`reconcile_aspect_if_unsorted_exceeds`](database::SegmentStore::reconcile_aspect_if_unsorted_exceeds),
/// so the rewrite runs only when the backlog is at or above `N`. Either way each
/// out-of-order segment is stable-sorted by timestamp and re-sealed in place at its
/// own id, so afterward point lookups over the aspect binary-search. A pass that
/// actually runs is counted in `dsp_reconcile_passes_total` (holds below the
/// threshold are not). Returns `200 OK` with whether it triggered, the number
/// rewritten, and the post-pass order-health count.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::NotFound`] when the aspect is undeclared, and
/// [`StorageError::Internal`] on a read/seal/control-plane failure.
pub async fn reconcile_aspect(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<ReconcileParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	let metrics = state.metrics().clone();
	drop(state);
	let result = reconcile_aspect_inner(&store, &aspect, params.threshold, params.hot_cold, params.overlaps, params.split_min_bytes, &metrics).await;
	drop(store);
	result
}

/// The body of [`reconcile_aspect`], split out so the significant-`Drop`
/// [`SegmentStore`](database::SegmentStore) handle is dropped in the caller after
/// the last use rather than held across the response construction.
async fn reconcile_aspect_inner(store: &database::SegmentStore, aspect: &str, threshold: Option<usize>, hot_cold: bool, overlaps: bool, split_min_bytes: Option<u64>, metrics: &crate::metrics::SharedMetrics) -> Result<Response, StorageError> {
	// Undeclared aspect → 404 (mirrors the read surface's not-found semantics).
	if store.schema_for(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?.is_none() {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
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
			Some(min) => store.reconcile_overlaps_with_policy(aspect, dsp_physical_type::SplitPolicy::new(min)).await.map_err(|err| StorageError::Internal(err.to_string()))?,
			None => store.reconcile_overlaps(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?,
		};
		("overlaps", true, removed, 0, 0)
	} else if hot_cold {
		let outcome = store.reconcile_aspect_hot_cold(aspect, threshold.unwrap_or(1)).await.map_err(|err| StorageError::Internal(err.to_string()))?;
		("hot_cold", true, outcome.total(), outcome.cold_reconciled, outcome.hot_reconciled)
	} else {
		let (triggered, reconciled) = match threshold {
			Some(threshold) => store.reconcile_aspect_if_unsorted_exceeds(aspect, threshold).await.map_err(|err| StorageError::Internal(err.to_string()))?.map_or((false, 0), |reconciled| (true, reconciled)),
			None => (true, store.reconcile_aspect(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?),
		};
		("threshold", triggered, reconciled, 0, 0)
	};
	match (mode, triggered, reconciled) {
		// overlaps / hot/cold: record a pass only when it actually changed something
		// (matches the background daemon / store-wide sweep semantics).
		("threshold", true, n) => metrics.record_reconcile_pass(u64::try_from(n).unwrap_or(u64::MAX)),
		("threshold", false, _) => {},
		(_, _, n) if n > 0 => metrics.record_reconcile_pass(u64::try_from(n).unwrap_or(u64::MAX)),
		(_, _, _) => {},
	}
	let stats = store.aspect_stats(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	Ok((StatusCode::OK, Json(ReconcileResponse { aspect: aspect.to_string(), mode, triggered, reconciled, cold_reconciled, hot_reconciled, unsorted_segments: stats.unsorted_segments, overlapping_segments: stats.overlapping_segments })).into_response())
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
}

/// Handle `POST /api/v1/storage/reconcile`: sweep **every** declared aspect,
/// reconciling those whose out-of-order backlog is at or above `?threshold=N`
/// (roadmap Phase 4.6).
///
/// The manual, store-wide operator counterpart to the per-aspect
/// [`reconcile_aspect`] endpoint and the background reconcile daemon — the same
/// [`SegmentStore::reconcile_all_over_threshold`](database::SegmentStore::reconcile_all_over_threshold)
/// sweep, on demand. An absent `threshold` defaults to 1 (reconcile any aspect with
/// out-of-order data). Each aspect actually reconciled is counted in
/// `dsp_reconcile_passes_total`, exactly like the per-aspect trigger and the daemon.
///
/// With `?hot_cold=true` it instead runs the
/// [`SegmentStore::reconcile_all_hot_cold`](database::SegmentStore::reconcile_all_hot_cold)
/// sweep: every aspect's cold segments are reconciled unconditionally and only the
/// hot tail is gated on the threshold. The response then carries the cold/hot split.
///
/// With `?overlaps=true` it runs the store-wide cross-segment overlap merge
/// ([`SegmentStore::reconcile_all_overlaps`](database::SegmentStore::reconcile_all_overlaps)),
/// merging each aspect's time-overlap groups; `segments_reconciled` is then the number
/// of segments merged away. `overlaps` takes precedence over `hot_cold`/`threshold`.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached, and
/// [`StorageError::Internal`] on a read/seal/control-plane failure.
pub async fn reconcile_store(State(state): State<AppState>, Query(params): Query<ReconcileParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	let metrics = state.metrics().clone();
	drop(state);
	let result = reconcile_store_inner(&store, params.threshold, params.hot_cold, params.overlaps, params.split_min_bytes, &metrics).await;
	drop(store);
	result
}

/// The body of [`reconcile_store`], split out so the significant-`Drop`
/// [`SegmentStore`](database::SegmentStore) handle is dropped in the caller.
async fn reconcile_store_inner(store: &database::SegmentStore, threshold: Option<usize>, hot_cold: bool, overlaps: bool, split_min_bytes: Option<u64>, metrics: &crate::metrics::SharedMetrics) -> Result<Response, StorageError> {
	let threshold = threshold.unwrap_or(1).max(1);
	let (mode, aspects_scanned, aspects_reconciled, segments_reconciled, cold_reconciled, hot_reconciled) = if overlaps {
		// `split_min_bytes` selects the store-wide split-not-rewrite floor; absent → the
		// default 50 MiB floor (every aspect's small components full-rewrite).
		let sweep = match split_min_bytes {
			Some(min) => store.reconcile_all_overlaps_with_policy(dsp_physical_type::SplitPolicy::new(min)).await.map_err(|err| StorageError::Internal(err.to_string()))?,
			None => store.reconcile_all_overlaps().await.map_err(|err| StorageError::Internal(err.to_string()))?,
		};
		if sweep.aspects_reconciled > 0 {
			metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_removed).unwrap_or(u64::MAX));
		}
		("overlaps", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_removed, 0, 0)
	} else if hot_cold {
		let sweep = store.reconcile_all_hot_cold(threshold).await.map_err(|err| StorageError::Internal(err.to_string()))?;
		if sweep.aspects_reconciled > 0 {
			metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_reconciled()).unwrap_or(u64::MAX));
		}
		("hot_cold", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_reconciled(), sweep.cold_reconciled, sweep.hot_reconciled)
	} else {
		let sweep = store.reconcile_all_over_threshold(threshold).await.map_err(|err| StorageError::Internal(err.to_string()))?;
		if sweep.aspects_reconciled > 0 {
			metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_reconciled).unwrap_or(u64::MAX));
		}
		("threshold", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_reconciled, 0, 0)
	};
	let unsorted_segments = store.store_stats().await.map_err(|err| StorageError::Internal(err.to_string()))?.unsorted_segments;
	let overlapping_segments = store.store_overlapping_segments().await.map_err(|err| StorageError::Internal(err.to_string()))?;
	Ok((StatusCode::OK, Json(ReconcileStoreResponse { threshold, mode, aspects_scanned, aspects_reconciled, segments_reconciled, cold_reconciled, hot_reconciled, unsorted_segments, overlapping_segments })).into_response())
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
/// ([`SegmentStore::squash_aspect`](database::SegmentStore::squash_aspect)); with
/// `?max_segments=N` it delegates to the threshold-gated
/// [`squash_aspect_if_exceeds`](database::SegmentStore::squash_aspect_if_exceeds), so
/// the rewrite runs only when the segment count exceeds `N` — the trigger that bounds
/// the fragmentation repeated split carve-offs create. Returns `200 OK` with whether
/// it triggered, how many segments it removed, and the post-pass segment count.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::NotFound`] when the aspect is undeclared, and
/// [`StorageError::Internal`] on a read/seal/control-plane failure.
pub async fn squash_aspect(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<SquashParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	// Undeclared aspect → 404 (mirrors the reconcile/read surfaces).
	if store.schema_for(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?.is_none() {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	}
	let (triggered, removed) = match params.max_segments {
		Some(max) => store.squash_aspect_if_exceeds(&aspect, max).await.map_err(|err| StorageError::Internal(err.to_string()))?.map_or((false, 0), |removed| (true, removed)),
		None => (true, store.squash_aspect(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?),
	};
	let segment_count = store.segment_count(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?;
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
/// The manual counterpart of the `DSP_COMPACT_TARGET_ROWS` daemon pass, delegating to
/// [`SegmentStore::squash_aspect_to_target_rows`](database::SegmentStore::squash_aspect_to_target_rows).
/// Returns `200 OK` with how many segments it removed and the post-pass segment count.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::BadRequest`] when `target_rows` is absent,
/// [`StorageError::NotFound`] when the aspect is undeclared, and
/// [`StorageError::Internal`] on a read/seal/control-plane failure.
pub async fn compact_aspect(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<CompactParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	drop(state);
	let target_rows = params.target_rows.ok_or_else(|| StorageError::BadRequest("compact requires a target_rows query parameter".to_string()))?;
	// Undeclared aspect → 404 (mirrors the reconcile/squash surfaces).
	if store.schema_for(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?.is_none() {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	}
	let removed = store.squash_aspect_to_target_rows(&aspect, target_rows).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	let segment_count = store.segment_count(&aspect).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	drop(store);
	Ok((StatusCode::OK, Json(CompactResponse { aspect, removed, segment_count })).into_response())
}

/// Query parameters for `POST /api/v1/storage/backup`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BackupParams {
	/// A subdirectory name for this snapshot under the backup root. Restricted to
	/// `[A-Za-z0-9._-]` (and not `.`/`..`) so an API caller can never traverse out of
	/// the backup root. Absent → a `backup-<unix_millis>` name is generated.
	pub label: Option<String>,
	/// How each snapshot copy is verified: `source` (default) cross-checks it against a
	/// fresh read of the live control plane — the strongest check, but it assumes a
	/// **quiescent** store; `snapshot` verifies the copy on its own terms (it opens and
	/// every row is readable) without re-reading the source, which is what an **online**
	/// backup taken while ingest continues needs. Anything else → `400`.
	pub verify: Option<String>,
}

/// One control-plane database's entry in a [`BackupResponse`].
#[derive(Debug, Clone, Serialize)]
pub struct BackupDbReport {
	/// The snapshot file name (e.g. `segment_index.db`).
	pub name: String,
	/// The number of user tables whose row counts were verified equal to the source.
	pub tables: usize,
	/// The number of rows verified in this database's snapshot.
	pub rows: i64,
	/// The on-disk size of this snapshot file in bytes.
	pub bytes: u64,
}

/// Response body for `POST /api/v1/storage/backup` — the outcome of an online
/// control-plane snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct BackupResponse {
	/// The directory the four snapshot files were written to.
	pub dir: String,
	/// Per-database verified snapshot reports (`segment_index`, `metadata`,
	/// `aspect_catalog`, `catalog`).
	pub databases: Vec<BackupDbReport>,
	/// Total rows verified across all four control-plane databases.
	pub total_rows: i64,
	/// Total on-disk size of the four snapshot files in bytes.
	pub total_bytes: u64,
	/// Which verification was applied (`source` or `snapshot`), so a caller knows what
	/// `total_rows` proves: equality with the live source, or the copy's own readable
	/// contents.
	pub verify: &'static str,
}

/// Parse the `?verify=` selector into a [`VerifyMode`].
///
/// `source` (the default) is the quiescent-store cross-check; `snapshot` is the
/// concurrent-write-safe copy-only check. Returns `None` for an unknown token so the
/// caller can answer `400` rather than silently picking a mode.
fn parse_verify_mode(token: &str) -> Option<VerifyMode> {
	match token.trim().to_ascii_lowercase().as_str() {
		"source" | "source_match" | "quiescent" => Some(VerifyMode::SourceMatch),
		"snapshot" | "snapshot_only" | "online" => Some(VerifyMode::SnapshotOnly),
		_ => None,
	}
}

/// The wire token for a [`VerifyMode`], echoed in [`BackupResponse::verify`].
const fn verify_token(mode: VerifyMode) -> &'static str {
	match mode {
		VerifyMode::SourceMatch => "source",
		VerifyMode::SnapshotOnly => "snapshot",
	}
}

/// Validate a caller-supplied backup `label`: non-empty, only `[A-Za-z0-9._-]`, and
/// neither `.` nor `..` — so it names a single fresh subdirectory under the backup root
/// and can never be an absolute path or a `../` traversal.
fn valid_backup_label(label: &str) -> bool {
	!label.is_empty() && label != "." && label != ".." && label.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Handle `POST /api/v1/storage/backup`: snapshot the store's four control-plane DBs.
///
/// Takes an online, consistent snapshot via Turso's `VACUUM INTO` (roadmap **Phase
/// 7.4**), verifying each copy before returning.
///
/// The snapshot lands in `<base>/<label>`, where `<base>` is `DSP_BACKUP_DIR` if set
/// else `<store_root>/backups`, and `<label>` is the (validated) `?label=` or a generated
/// `backup-<unix_millis>`. Each destination file must be fresh (`VACUUM INTO` needs a
/// non-existing file), so a directory that already exists is rejected. The `.dspseg`
/// measurement frames are **not** part of this backup — control plane only, per the
/// storage boundary (hard-constraint #3).
///
/// # Errors
///
/// [`StorageError::Unconfigured`] when no store is attached,
/// [`StorageError::BadRequest`] when `label` is malformed or the target dir already
/// exists, and [`StorageError::Internal`] on a backup/verify failure.
pub async fn backup_store(State(state): State<AppState>, Query(params): Query<BackupParams>) -> Result<Response, StorageError> {
	let store = state.store().cloned().ok_or(StorageError::Unconfigured)?;
	let metrics = state.metrics().clone();
	drop(state);
	let mode = match params.verify.as_deref() {
		Some(token) => parse_verify_mode(token).ok_or_else(|| StorageError::BadRequest(format!("unknown verify mode `{token}` — use `source` (quiescent cross-check) or `snapshot` (concurrent-write-safe copy-only check)")))?,
		None => VerifyMode::default(),
	};
	let base = std::env::var_os("DSP_BACKUP_DIR").map_or_else(|| store.root().join("backups"), std::path::PathBuf::from);
	let sub = if let Some(label) = params.label {
		if !valid_backup_label(&label) {
			return Err(StorageError::BadRequest(format!("invalid backup label `{label}` — use only letters, digits, '.', '_', '-'")));
		}
		label
	} else {
		let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|err| StorageError::Internal(err.to_string()))?.as_millis();
		format!("backup-{millis}")
	};
	let dest = base.join(&sub);
	if dest.exists() {
		return Err(StorageError::BadRequest(format!("backup target already exists: {} (choose a fresh label)", dest.display())));
	}
	let backup = store.backup_control_plane_with_verify(&dest, mode).await.map_err(|err| StorageError::Internal(err.to_string()))?;
	drop(store);
	metrics.record_backup(backup.total_bytes());
	let databases = [("segment_index.db", &backup.segment_index), ("metadata.db", &backup.metadata), ("aspect_catalog.db", &backup.aspect_catalog), ("catalog.db", &backup.registry)]
		.into_iter()
		.map(|(name, report)| BackupDbReport { name: name.to_string(), tables: report.tables, rows: report.rows, bytes: report.bytes })
		.collect();
	let response = BackupResponse { dir: backup.dir.display().to_string(), databases, total_rows: backup.total_rows(), total_bytes: backup.total_bytes(), verify: verify_token(mode) };
	Ok((StatusCode::OK, Json(response)).into_response())
}

/// One point in an ingest batch: an integer epoch timestamp (in the aspect's
/// declared [`TimeUnit`]) and its value as lossless decimal text, or `null` for an
/// absent/null row.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestPoint {
	/// Row timestamp (epoch integer in the aspect's declared unit).
	pub timestamp: i64,
	/// The value as decimal text (no float round-trip — hard constraint #4), or
	/// `null` for a null row (sealed into the segment's quality mask).
	pub value: Option<String>,
}

/// Request body for `POST /api/v1/storage/{aspect}/points` — seal a batch of points
/// into the aspect's declared schema.
///
/// When `rows_per_page` is set the batch seals into a **paged** segment (rows
/// partitioned into pages of that height for intra-segment page skipping);
/// otherwise it seals into a single-block segment.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestRequest {
	/// The points to seal, in caller order (the seal records whether they are
	/// time-sorted).
	pub points: Vec<IngestPoint>,
	/// Optional page height — when present, seal a paged segment of this many rows
	/// per page (the last page may be shorter).
	#[serde(default)]
	pub rows_per_page: Option<usize>,
	/// When `true`, reject the batch (`400`) if its timestamps are not monotonic
	/// non-decreasing, instead of sealing an out-of-order segment. Defaults to
	/// `false` (out-of-order data is accepted and flagged for the Phase-4.6
	/// reconciliation path). Equal adjacent timestamps are in order.
	#[serde(default)]
	pub require_sorted: bool,
}

/// Response body for a successful ingest: the sealed segment's descriptor summary,
/// the inputs a later read prunes on.
#[derive(Debug, Clone, Serialize)]
pub struct IngestResponse {
	/// The aspect the batch was sealed into.
	pub aspect: String,
	/// The id assigned to the new segment (monotonic within the aspect).
	pub segment_id: u64,
	/// The `.dspseg` frame format version sealed (5 single-block, 6 paged).
	pub format_version: u16,
	/// Total rows sealed (present and null).
	pub row_count: usize,
	/// Null rows sealed.
	pub null_count: usize,
	/// The realized on-disk frame size in bytes (the bytes/point numerator).
	pub byte_len: u64,
	/// Smallest timestamp in the sealed segment, or `null` if it was empty.
	pub min_ts: Option<i64>,
	/// Largest timestamp in the sealed segment, or `null` if it was empty.
	pub max_ts: Option<i64>,
	/// Whether the sealed segment's timestamps are monotonic non-decreasing. `false`
	/// flags out-of-order data that a point lookup must linear-scan (Phase 4.6) — a
	/// client ingesting without `require_sorted` can watch this to know whether its
	/// batch stored in ordered form. Always `true` for a batch accepted under
	/// `require_sorted`.
	pub time_sorted: bool,
}

/// Classify a seal error: an encode/tolerance failure is a client-data problem
/// (the supplied values cannot be represented under the aspect's declared encoding
/// within its tolerance — hard constraint #4) → `400`; anything else (filesystem,
/// libSQL) is a `500`.
fn classify_seal_error(err: &anyhow::Error) -> StorageError {
	let message = err.to_string();
	if message.contains("seal failed") || message.contains("paged seal failed") {
		StorageError::BadRequest(message)
	} else {
		StorageError::Internal(message)
	}
}

/// Enforce monotonic non-decreasing timestamps when a request opts in
/// (`require_sorted`) — the API surface of the Phase-4.2 order enforcement
/// ([`dsp_physical_type::first_order_violation`] / `AspectSchema::seal_sorted`).
///
/// A client ingesting an append-only stream can demand the server reject an
/// out-of-order batch (which would otherwise seal an unsortable segment that forces
/// linear scans on read) rather than silently store it. Returns
/// [`StorageError::BadRequest`] naming the first backwards row; a `false` flag (the
/// default) is a no-op so the permissive path is unchanged.
fn enforce_order_if_required(require_sorted: bool, timestamps: &[i64]) -> Result<(), StorageError> {
	if require_sorted {
		if let Some((index, previous, current)) = first_order_violation(timestamps) {
			return Err(StorageError::BadRequest(format!("out-of-order timestamp at row {index}: {current} < previous {previous} (require_sorted was set)")));
		}
	}
	Ok(())
}

/// Handle `POST /api/v1/storage/{aspect}/points`: seal a batch of points into
/// `aspect`'s declared schema.
///
/// Splits the batch into a dense timestamp column and an `Option`-valued value
/// column; a batch with any `null` value seals through the nullable (quality-mask)
/// path, an all-present batch through the dense path; `rows_per_page` selects a
/// paged frame. Returns `201 Created` with the sealed segment's descriptor.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] (no store), [`StorageError::NotFound`] (aspect
/// undeclared), [`StorageError::BadRequest`] (empty batch, an unparseable value, or
/// values unrepresentable under the declared encoding/tolerance), and
/// [`StorageError::Internal`] on a filesystem/control-plane failure.
pub async fn ingest_points(State(state): State<AppState>, Path(aspect): Path<String>, Json(request): Json<IngestRequest>) -> Result<Response, StorageError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| { metrics.record_ingest_error(); StorageError::Unconfigured })?;
	drop(state);
	// Time the seal path (not the store-unconfigured fast-fail above): this is the
	// ingest side of the north-star "predictable p95/p99 under ingest + query".
	let start = std::time::Instant::now();
	let result = ingest_points_inner(&store, &metrics, &aspect, request).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// The body of [`ingest_points`], split out so the handler can record an error
/// metric for any failure path uniformly.
async fn ingest_points_inner(store: &database::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, request: IngestRequest) -> Result<Response, StorageError> {
	if request.points.is_empty() {
		return Err(StorageError::BadRequest("no points to ingest (`points` is empty)".to_string()));
	}
	// An undeclared aspect is a clean 404 (its encoding is unknown) rather than the
	// seal's generic error; fetching the schema here also lets the nullable/paged
	// seal variants take it directly.
	let Some(schema) = store.schema_for(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))? else {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};

	let point_count = request.points.len();
	let mut timestamps = Vec::with_capacity(point_count);
	let mut values = Vec::with_capacity(point_count);
	let mut any_null = false;
	// `storage.ingest.parse` child span (roadmap Phase 3 — the write-path analogue of
	// the compute-path parse span): the decimal-text → `BigDecimal` value lift of the
	// batch, timed apart from the seal so a `RUST_LOG` run attributes ingest value
	// parsing separately from the segment write.
	{
		let _parse = tracing::info_span!("storage.ingest.parse", point_count, format = "json").entered();
		for point in &request.points {
			timestamps.push(point.timestamp);
			match &point.value {
				None => {
					any_null = true;
					values.push(None);
				}
				Some(text) => {
					let parsed: BigDecimal = text.parse().map_err(|_| StorageError::BadRequest(format!("value {text:?} (at timestamp {}) is not a decimal", point.timestamp)))?;
					values.push(Some(parsed));
				}
			}
		}
	}

	enforce_order_if_required(request.require_sorted, &timestamps)?;
	// `storage.ingest.seal` child span (roadmap Phase 3): the typed-column encode +
	// `.dspseg` write + control-plane index update — the write stage timed apart from
	// value parsing.
	let descriptor = seal_batch(store, aspect, &schema, &timestamps, &values, any_null, request.rows_per_page).instrument(tracing::info_span!("storage.ingest.seal", point_count, any_null, format = "json")).await.map_err(|err| classify_seal_error(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse {
		aspect: aspect.to_string(),
		segment_id: descriptor.id,
		format_version: descriptor.format_version,
		row_count: descriptor.row_count,
		null_count: descriptor.null_count,
		byte_len: descriptor.byte_len,
		min_ts: descriptor.min_ts,
		max_ts: descriptor.max_ts,
		time_sorted: descriptor.time_sorted,
	};
	Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// Dispatch the right seal path for the batch: nullable vs dense × paged vs
/// single-block. Kept as a free async fn taking `&SegmentStore` so the handler can
/// drop its store handle before building the response.
async fn seal_batch(store: &database::SegmentStore, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>], any_null: bool, rows_per_page: Option<usize>) -> anyhow::Result<SegmentDescriptor> {
	match (rows_per_page, any_null) {
		(Some(rows_per_page), true) => store.seal_paged_nullable(aspect, schema, timestamps, values, rows_per_page).await,
		(Some(rows_per_page), false) => store.seal_paged(aspect, schema, timestamps, &present_values(values), rows_per_page).await,
		(None, true) => store.seal_nullable(aspect, schema, timestamps, values).await,
		(None, false) => store.seal(aspect, schema, timestamps, &present_values(values)).await,
	}
}

/// Unwrap an all-present value column to the dense `BigDecimal` slice the dense seal
/// paths take. Only called when the caller has verified no value is `None`.
fn present_values(values: &[Option<BigDecimal>]) -> Vec<BigDecimal> {
	values.iter().map(|value| value.clone().unwrap_or_default()).collect()
}

/// Query parameters for the CSV ingest endpoint: an optional page height.
#[derive(Debug, Clone, Deserialize)]
pub struct CsvIngestParams {
	/// Optional page height — seal a paged segment of this many rows per page.
	#[serde(default)]
	pub rows_per_page: Option<usize>,
	/// When `true`, reject an out-of-order batch (`400`) instead of sealing it; see
	/// [`IngestRequest::require_sorted`].
	#[serde(default)]
	pub require_sorted: bool,
}

/// The dense columns parsed from a CSV ingest body: aligned timestamp and
/// `Option`-value vectors plus whether any value is null (selecting the
/// nullable seal path).
struct ParsedCsv {
	/// Row timestamps in caller order (epoch integers in the aspect's declared unit).
	timestamps: Vec<i64>,
	/// Per-row values, `None` for a null (empty) field.
	values: Vec<Option<BigDecimal>>,
	/// Whether any value is null (selects the quality-mask seal path).
	any_null: bool,
}

/// Parse a `timestamp,value` CSV body into dense timestamp + `Option`-value columns.
///
/// The inverse of the [`storage`](crate::storage) CSV export: each non-blank line
/// is `integer-epoch,decimal-text`, an **empty** value field is a null row, and an
/// optional leading `timestamp,value` header line is skipped. Values are parsed
/// losslessly as `BigDecimal` (no float round-trip — hard constraint #4). Only the
/// first comma splits the line, so a value never needs quoting (it is plain decimal
/// text), matching the export's no-escaping guarantee.
///
/// # Errors
///
/// [`StorageError::BadRequest`] for a line without a comma, a non-integer
/// timestamp, an unparseable value, or an empty body (no data rows).
fn parse_csv_points(body: &str) -> Result<ParsedCsv, StorageError> {
	let mut timestamps = Vec::new();
	let mut values = Vec::new();
	let mut any_null = false;
	for (index, raw) in body.lines().enumerate() {
		let line = raw.trim();
		if line.is_empty() {
			continue;
		}
		let (ts_text, value_text) = line.split_once(',').ok_or_else(|| StorageError::BadRequest(format!("CSV line {} has no comma separator: {raw:?}", index + 1)))?;
		let ts_text = ts_text.trim();
		let value_text = value_text.trim();
		// Skip an optional leading header row (`timestamp,value`).
		if timestamps.is_empty() && ts_text.eq_ignore_ascii_case("timestamp") {
			continue;
		}
		let timestamp: i64 = ts_text.parse().map_err(|_| StorageError::BadRequest(format!("CSV line {}: timestamp {ts_text:?} is not an integer", index + 1)))?;
		if value_text.is_empty() {
			any_null = true;
			values.push(None);
		} else {
			let parsed: BigDecimal = value_text.parse().map_err(|_| StorageError::BadRequest(format!("CSV line {}: value {value_text:?} is not a decimal", index + 1)))?;
			values.push(Some(parsed));
		}
		timestamps.push(timestamp);
	}
	if timestamps.is_empty() {
		return Err(StorageError::BadRequest("no CSV rows to ingest".to_string()));
	}
	Ok(ParsedCsv { timestamps, values, any_null })
}

/// Handle `POST /api/v1/storage/{aspect}/csv?rows_per_page`.
///
/// The CSV counterpart of the JSON / ILP / Parquet ingest endpoints and the
/// write-side of the [`storage`](crate::storage) CSV export: parses a
/// `timestamp,value` CSV body (the request body, `text/csv` or `text/plain`) and
/// seals it into `aspect`'s declared schema. An empty value field seals a null row
/// (through the quality-mask path); `rows_per_page` selects a paged frame. The same
/// no-silent-downcast guarantee holds — a value unrepresentable under the declared
/// encoding/tolerance is rejected `400`, never downcast (hard constraint #4).
///
/// # Errors
///
/// [`StorageError::Unconfigured`] (no store), [`StorageError::NotFound`] (aspect
/// undeclared), [`StorageError::BadRequest`] (malformed CSV, empty body, or values
/// unrepresentable under the declared encoding/tolerance), and
/// [`StorageError::Internal`] on a filesystem/control-plane failure.
pub async fn ingest_csv(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<CsvIngestParams>, body: String) -> Result<Response, StorageError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| { metrics.record_ingest_error(); StorageError::Unconfigured })?;
	drop(state);
	let start = std::time::Instant::now();
	let result = ingest_csv_inner(&store, &metrics, &aspect, params.rows_per_page, params.require_sorted, &body).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// The body of [`ingest_csv`], split out so the handler records an error metric for
/// any failure path uniformly. Shares [`seal_batch`] / [`classify_seal_error`] with
/// the JSON ingest path, so a CSV-sealed batch is byte-identical to a JSON-sealed one.
async fn ingest_csv_inner(store: &database::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, rows_per_page: Option<usize>, require_sorted: bool, body: &str) -> Result<Response, StorageError> {
	let Some(schema) = store.schema_for(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))? else {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	// `storage.ingest.parse` decode span (roadmap Phase 3): the CSV text → typed
	// `(timestamp, value)` columns, timed apart from the seal.
	let ParsedCsv { timestamps, values, any_null } = tracing::info_span!("storage.ingest.parse", format = "csv").in_scope(|| parse_csv_points(body))?;
	enforce_order_if_required(require_sorted, &timestamps)?;
	let point_count = timestamps.len();
	let descriptor = seal_batch(store, aspect, &schema, &timestamps, &values, any_null, rows_per_page).instrument(tracing::info_span!("storage.ingest.seal", point_count, any_null, format = "csv")).await.map_err(|err| classify_seal_error(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse {
		aspect: aspect.to_string(),
		segment_id: descriptor.id,
		format_version: descriptor.format_version,
		row_count: descriptor.row_count,
		null_count: descriptor.null_count,
		byte_len: descriptor.byte_len,
		min_ts: descriptor.min_ts,
		max_ts: descriptor.max_ts,
		time_sorted: descriptor.time_sorted,
	};
	Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// Query parameters for the ILP ingest endpoint: which field to seal, the wire
/// timestamp precision, and an optional page height.
#[derive(Debug, Clone, Deserialize)]
pub struct IlpIngestParams {
	/// The line-protocol field key whose numeric values are sealed (one aspect ==
	/// one field).
	pub field: String,
	/// The wire timestamp precision token (`ns`/`us`/`ms`/`s`, default `ns`) —
	/// how the integer timestamps **in the payload** are interpreted. The stored
	/// epochs are then rescaled to the aspect's declared [`TimeUnit`].
	#[serde(default)]
	pub precision: Option<String>,
	/// Optional page height — seal a paged segment of this many rows per page.
	#[serde(default)]
	pub rows_per_page: Option<usize>,
	/// When `true`, reject an out-of-order batch (`400`) instead of sealing it; see
	/// [`IngestRequest::require_sorted`]. Order is checked on the epochs after they
	/// are rescaled to the aspect's declared unit.
	#[serde(default)]
	pub require_sorted: bool,
}

/// Map the optional ILP precision token to a [`TimestampPrecision`] (default
/// nanoseconds), mirroring the interpolation ILP endpoint's accepted tokens.
///
/// # Errors
///
/// Returns a message naming the unknown token.
fn parse_ilp_precision(token: Option<&str>) -> Result<TimestampPrecision, String> {
	match token {
		None | Some("ns" | "nanoseconds" | "nanos") => Ok(TimestampPrecision::Nanoseconds),
		Some("us" | "µs" | "microseconds" | "micros") => Ok(TimestampPrecision::Microseconds),
		Some("ms" | "milliseconds" | "millis") => Ok(TimestampPrecision::Milliseconds),
		Some("s" | "sec" | "secs" | "seconds") => Ok(TimestampPrecision::Seconds),
		Some(other) => Err(format!("unknown precision token {other:?} (expected ns, us, ms, or s)")),
	}
}

/// Project an absolute instant onto the integer epoch the store keeps for `unit`.
///
/// The line-protocol parser yields an absolute [`DateTime<Utc>`]; the store keeps
/// integer epochs in the aspect's **declared** unit, so the wire precision and the
/// stored resolution can differ (an `ns`-precision payload sealed into a
/// seconds-resolution aspect, say). Returns [`None`] only for the nanosecond unit
/// when the instant falls outside the `i64`-nanosecond range (before 1677 or after
/// 2262).
const fn epoch_in_unit(instant: DateTime<Utc>, unit: TimeUnit) -> Option<i64> {
	match unit {
		TimeUnit::Seconds => Some(instant.timestamp()),
		TimeUnit::Millis => Some(instant.timestamp_millis()),
		TimeUnit::Micros => Some(instant.timestamp_micros()),
		TimeUnit::Nanos => instant.timestamp_nanos_opt(),
	}
}

/// Handle `POST /api/v1/storage/{aspect}/ilp`.
///
/// Parses an InfluxDB-Line-Protocol payload (the wire format TSBS / `InfluxDB` /
/// `QuestDB` speak) and seals the chosen field's values into `aspect`'s declared
/// schema.
///
/// The payload is the request body (`text/plain`); `field`, `precision`, and
/// `rows_per_page` are query parameters. The parser is the shared, vendor-neutral
/// `dsp-line-protocol` crate, so the storage ingest path and the interpolation ILP
/// path accept the exact same dialect. Each point's absolute instant is rescaled to
/// the aspect's declared [`TimeUnit`] before sealing. ILP fields are always present,
/// so this is a dense seal.
///
/// # Errors
///
/// [`StorageError::Unconfigured`] (no store), [`StorageError::NotFound`] (aspect
/// undeclared), [`StorageError::BadRequest`] (unknown precision token, malformed
/// payload, no points carrying the field, a timestamp outside the declared unit's
/// range, or values unrepresentable under the declared encoding/tolerance), and
/// [`StorageError::Internal`] on a filesystem/control-plane failure.
pub async fn ingest_ilp(State(state): State<AppState>, Path(aspect): Path<String>, Query(params): Query<IlpIngestParams>, body: String) -> Result<Response, StorageError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| { metrics.record_ingest_error(); StorageError::Unconfigured })?;
	drop(state);
	let start = std::time::Instant::now();
	let result = ingest_ilp_inner(&store, &metrics, &aspect, &params, &body).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// The body of [`ingest_ilp`], split out so the handler records an error metric for
/// any failure path uniformly.
async fn ingest_ilp_inner(store: &database::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, params: &IlpIngestParams, body: &str) -> Result<Response, StorageError> {
	let precision = parse_ilp_precision(params.precision.as_deref()).map_err(StorageError::BadRequest)?;
	let Some(schema) = store.schema_for(aspect).await.map_err(|err| StorageError::Internal(err.to_string()))? else {
		return Err(StorageError::NotFound(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	// `storage.ingest.parse` decode span (roadmap Phase 3): the line-protocol text →
	// typed points, timed apart from the timestamp normalization and the seal.
	let points = tracing::info_span!("storage.ingest.parse", format = "ilp", field = %params.field).in_scope(|| dsp_line_protocol::parse_points(body, &params.field, precision)).map_err(|err| StorageError::BadRequest(err.to_string()))?;
	if points.is_empty() {
		return Err(StorageError::BadRequest(format!("no points carrying field `{}` with a timestamp in the payload", params.field)));
	}

	// `storage.ingest.normalize` span (roadmap Phase 3): rescale each wire epoch to the
	// aspect's declared `TimeUnit` — the timestamp-normalization stage, distinct from
	// the wire decode above.
	let (timestamps, values) = {
		let _normalize = tracing::info_span!("storage.ingest.normalize", point_count = points.len(), unit = %schema.timestamp_unit.name()).entered();
		let mut timestamps = Vec::with_capacity(points.len());
		let mut values = Vec::with_capacity(points.len());
		for point in points {
			let epoch = epoch_in_unit(point.timestamp, schema.timestamp_unit).ok_or_else(|| StorageError::BadRequest(format!("timestamp {} is outside the range of the declared `{}` unit", point.timestamp, schema.timestamp_unit.name())))?;
			timestamps.push(epoch);
			values.push(point.value);
		}
		(timestamps, values)
	};

	enforce_order_if_required(params.require_sorted, &timestamps)?;
	let point_count = timestamps.len();
	let seal_span = tracing::info_span!("storage.ingest.seal", point_count, format = "ilp");
	let descriptor = match params.rows_per_page {
		Some(rows_per_page) => store.seal_paged(aspect, &schema, &timestamps, &values, rows_per_page).instrument(seal_span).await,
		None => store.seal(aspect, &schema, &timestamps, &values).instrument(seal_span).await,
	}
	.map_err(|err| classify_seal_error(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse {
		aspect: aspect.to_string(),
		segment_id: descriptor.id,
		format_version: descriptor.format_version,
		row_count: descriptor.row_count,
		null_count: descriptor.null_count,
		byte_len: descriptor.byte_len,
		min_ts: descriptor.min_ts,
		max_ts: descriptor.max_ts,
		time_sorted: descriptor.time_sorted,
	};
	Ok((StatusCode::CREATED, Json(response)).into_response())
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use database::SegmentStore;
	use tempfile::TempDir;
	use tower::ServiceExt;

	use crate::{app_with_state, AppState};

	/// Build a router over a fresh, empty store under `dir` (no aspects declared).
	async fn router_with_empty_store(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		app_with_state(AppState::new().with_store(store))
	}

	/// POST `body` (a JSON value) to `uri`, returning the status and parsed JSON body.
	async fn post_json(router: axum::Router, uri: &str, body: &serde_json::Value) -> (StatusCode, serde_json::Value) {
		let response = router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(body).unwrap())).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
	}

	#[tokio::test]
	async fn declare_creates_an_aspect_the_read_surface_lists() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "price", "physical_type": "f64", "timestamp_unit": "seconds" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"]["name"], "price");
		assert_eq!(body["aspect"]["physical_type"], "f64");
		assert_eq!(body["aspect"]["timestamp_unit"], "seconds");
		// value_tolerance defaulted to exact.
		assert_eq!(body["aspect"]["value_tolerance"], "0");

		// And the read surface now lists it.
		let router = router_with_empty_store(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/aspects").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(listed["aspects"][0]["name"], "price");
	}

	#[tokio::test]
	async fn declare_scaled_i64_requires_scale() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "temp", "physical_type": "scaled_i64", "timestamp_unit": "millis" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("requires a `scale`"));
	}

	#[tokio::test]
	async fn declare_scaled_i64_with_scale_succeeds() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "temp", "physical_type": "scaled_i64", "scale": 2, "timestamp_unit": "millis" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"]["physical_type"], "scaled_i64");
		assert_eq!(body["aspect"]["timestamp_unit"], "millis");
	}

	#[tokio::test]
	async fn declare_unknown_physical_type_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "x", "physical_type": "float", "timestamp_unit": "seconds" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("unknown physical_type"));
	}

	#[tokio::test]
	async fn declare_unknown_timestamp_unit_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "name": "x", "physical_type": "f64", "timestamp_unit": "fortnights" });
		let (status, body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("unknown timestamp_unit"));
	}

	#[tokio::test]
	async fn declare_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let body = serde_json::json!({ "name": "x", "physical_type": "f64", "timestamp_unit": "seconds" });
		let (status, _body) = post_json(router, "/api/v1/storage/aspects", &body).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}

	/// Declare `price` (F64, seconds) in a fresh store under `dir`, returning a router
	/// over that store.
	async fn router_with_declared_price(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		store.declare("price", &dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds)).await.expect("declares");
		app_with_state(AppState::new().with_store(store))
	}

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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
	}

	#[tokio::test]
	async fn compact_endpoint_coalesces_toward_target_and_requires_the_param() {
		let dir = TempDir::new().unwrap();
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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

	#[test]
	fn valid_backup_label_rejects_traversal_and_bad_chars() {
		use super::valid_backup_label;
		assert!(valid_backup_label("nightly"));
		assert!(valid_backup_label("2026-07-21_run.1"));
		assert!(!valid_backup_label(""));
		assert!(!valid_backup_label("."));
		assert!(!valid_backup_label(".."));
		assert!(!valid_backup_label("a/b"));
		assert!(!valid_backup_label("a\\b"));
		assert!(!valid_backup_label("a b"));
	}

	#[tokio::test]
	async fn backup_endpoint_snapshots_control_plane_and_guards_label() {
		let dir = TempDir::new().unwrap();
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens"));
		store.declare("price", &sc).await.expect("declares");
		store.seal("price", &sc, &[0_i64, 10, 20], &["1".parse().unwrap(), "2".parse().unwrap(), "3".parse().unwrap()]).await.expect("seals");

		// One shared state so the /metrics counter reflects the backup below.
		let state = AppState::new().with_store(store.clone());

		// A traversal label is rejected before touching disk.
		let router = app_with_state(state.clone());
		let bad = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=..").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "traversal label rejected");

		// A valid label snapshots all four control-plane DBs under <root>/backups/<label>.
		let router = app_with_state(state.clone());
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=nightly").body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(status, StatusCode::OK, "body: {json}");
		assert_eq!(json["databases"].as_array().unwrap().len(), 4, "four control-plane DBs snapshotted");
		assert!(json["total_rows"].as_i64().unwrap() >= 2, "at least the seal's index + rollup rows");
		assert!(json["total_bytes"].as_u64().unwrap() > 0, "the snapshot files have a non-zero footprint");
		assert!(json["databases"][0]["bytes"].as_u64().unwrap() > 0, "each snapshot file reports its size");
		let backup_dir = dir.path().join("backups").join("nightly");
		for name in ["segment_index.db", "metadata.db", "aspect_catalog.db", "catalog.db"] {
			assert!(backup_dir.join(name).exists(), "{name} written to disk");
		}

		// Re-using the same label collides with the existing dir → 400 (VACUUM INTO needs a fresh file).
		let router = app_with_state(state.clone());
		let dup = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=nightly").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(dup.status(), StatusCode::BAD_REQUEST, "an existing backup dir is rejected");

		// /metrics reflects the one successful snapshot (the failed dup did not bump it).
		let router = app_with_state(state.clone());
		let metrics = router.oneshot(Request::builder().method("GET").uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
		let mbytes = axum::body::to_bytes(metrics.into_body(), usize::MAX).await.unwrap();
		let mtext = String::from_utf8(mbytes.to_vec()).unwrap();
		assert!(mtext.contains("dsp_backup_snapshots_total 1"), "one snapshot recorded; got:\n{mtext}");
		assert!(mtext.contains("dsp_backup_bytes_written_total"), "bytes-written counter present");
		assert_eq!(json["verify"], "source", "the default verification is the quiescent source cross-check");

		// The concurrent-write-safe mode verifies each copy on its own terms (no source
		// re-read) and says so in the response — what an online backup needs.
		let router = app_with_state(state.clone());
		let online = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=online&verify=snapshot").body(Body::empty()).unwrap()).await.unwrap();
		let ostatus = online.status();
		let obytes = axum::body::to_bytes(online.into_body(), usize::MAX).await.unwrap();
		let ojson: serde_json::Value = serde_json::from_slice(&obytes).unwrap();
		assert_eq!(ostatus, StatusCode::OK, "body: {ojson}");
		assert_eq!(ojson["verify"], "snapshot", "the response says which check was applied");
		assert_eq!(ojson["databases"].as_array().unwrap().len(), 4);
		assert!(ojson["total_rows"].as_i64().unwrap() >= 2, "rows scanned out of the copies themselves");

		// An unknown verify token is a 400 rather than a silent default, and writes nothing.
		let router = app_with_state(state.clone());
		let bad_mode = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/backup?label=other&verify=maybe").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(bad_mode.status(), StatusCode::BAD_REQUEST, "unknown verify mode rejected");
		assert!(!dir.path().join("backups").join("other").exists(), "a rejected request writes nothing");
		drop(store);
	}

	#[test]
	fn verify_mode_tokens_round_trip_and_reject_junk() {
		use database::VerifyMode;

		use super::{parse_verify_mode, verify_token};
		assert_eq!(parse_verify_mode("source"), Some(VerifyMode::SourceMatch));
		assert_eq!(parse_verify_mode("QUIESCENT"), Some(VerifyMode::SourceMatch));
		assert_eq!(parse_verify_mode(" snapshot "), Some(VerifyMode::SnapshotOnly));
		assert_eq!(parse_verify_mode("online"), Some(VerifyMode::SnapshotOnly));
		assert_eq!(parse_verify_mode("yes"), None, "an unknown token must 400, not silently pick a mode");
		assert_eq!(verify_token(VerifyMode::SourceMatch), "source");
		assert_eq!(verify_token(VerifyMode::SnapshotOnly), "snapshot");
	}

	#[tokio::test]
	async fn reconcile_store_endpoint_overlaps_merges_across_aspects() {
		let dir = TempDir::new().unwrap();
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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
		let sc = dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds);
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

	#[tokio::test]
	async fn ingest_seals_a_batch_the_read_surface_returns() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"], "price");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 0);
		assert_eq!(body["min_ts"], 100);
		assert_eq!(body["max_ts"], 120);
		assert_eq!(body["time_sorted"], true, "an ordered batch seals sorted");
		assert!(body["byte_len"].as_u64().unwrap() > 0);

		// The read surface returns exactly what was sealed.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/points?start=100&end=120").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let read: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(read["count"], 3);
		assert_eq!(read["points"][1]["value"], "2.5");
	}

	/// Reopen a router over an existing store dir (no re-declaration — the catalog
	/// persists), for reading back what a prior request sealed.
	async fn router_with_declared_price_reopened(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("reopens store"));
		app_with_state(AppState::new().with_store(store))
	}

	#[tokio::test]
	async fn ingest_nullable_batch_records_null_count() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": null },
			{ "timestamp": 120, "value": "3.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 1);
	}

	#[tokio::test]
	async fn ingest_paged_batch_seals_paged_frame() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "rows_per_page": 2, "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
			{ "timestamp": 130, "value": "4.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		// Paged frames carry the paged format version, distinct from a single-block frame.
		assert_eq!(body["format_version"], dsp_physical_type::PAGED_SEGMENT_FORMAT_VERSION);
		assert_eq!(body["row_count"], 4);
	}

	#[tokio::test]
	async fn ingest_into_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1" }] });
		let (status, body) = post_json(router, "/api/v1/storage/ghost/points", &body).await;
		assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
	}

	#[tokio::test]
	async fn ingest_unparseable_value_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "not-a-number" }] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("is not a decimal"));
	}

	#[tokio::test]
	async fn ingest_empty_batch_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("no points"));
	}

	#[tokio::test]
	async fn ingest_value_exceeding_tolerance_is_rejected() {
		// Declare a scaled_i64 with scale 0 (integers only) and tolerance 0: a
		// fractional value cannot be represented exactly and must be rejected, not
		// silently downcast (hard constraint #4).
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.unwrap());
		store.declare("counts", &dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::ScaledI64 { scale: 0 }, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds)).await.unwrap();
		let router = app_with_state(AppState::new().with_store(store));
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1.5" }] });
		let (status, body) = post_json(router, "/api/v1/storage/counts/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("seal failed"));
	}

	#[tokio::test]
	async fn ingest_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1" }] });
		let (status, _body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn ingest_require_sorted_rejects_out_of_order_but_default_accepts() {
		let dir = TempDir::new().unwrap();
		// require_sorted: a backwards timestamp (row 1: 90 < 100) is a 400 naming the row.
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "require_sorted": true, "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 90, "value": "2.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("out-of-order timestamp at row 1"), "body: {body}");

		// The same batch without the flag is accepted (out-of-order data is legal by default).
		let router = router_with_declared_price_reopened(&dir).await;
		let ok = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 90, "value": "2.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &ok).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["row_count"], 2);
		// The accepted out-of-order batch is honestly reported as unsorted.
		assert_eq!(body["time_sorted"], false, "an out-of-order batch reports time_sorted=false");
	}

	#[tokio::test]
	async fn ingest_require_sorted_admits_equal_and_ascending() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		// Equal adjacent timestamps are in order; ascending is fine.
		let body = serde_json::json!({ "require_sorted": true, "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 100, "value": "2.5" },
			{ "timestamp": 110, "value": "3.5" },
		] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["row_count"], 3);
	}

	#[tokio::test]
	async fn csv_ingest_require_sorted_rejects_out_of_order() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv?require_sorted=true", "100,1.5\n90,2.5\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("out-of-order timestamp"), "body: {body}");
	}

	#[tokio::test]
	async fn ilp_ingest_require_sorted_passes_because_the_parser_pre_sorts() {
		// The `dsp-line-protocol` parser returns points sorted by timestamp, so an
		// out-of-order *payload* still seals an in-order segment — `require_sorted`
		// therefore accepts it. This locks in that interaction (order enforcement on
		// the ILP path is a guard against a future non-sorting parser, not a rejection
		// of a shuffled payload).
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		let payload = "weather temp=1.5 100\nweather temp=2.5 90\n";
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?field=temp&precision=s&require_sorted=true", payload).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["min_ts"], 90);
		assert_eq!(body["max_ts"], 100);
	}

	/// POST a `text/plain` ILP payload to `uri`, returning status and parsed JSON.
	async fn post_text(router: axum::Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
		let response = router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "text/plain").body(Body::from(body.to_string())).unwrap()).await.unwrap();
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
	}

	#[tokio::test]
	async fn ilp_ingest_seals_the_chosen_field() {
		let dir = TempDir::new().unwrap();
		// Declare `temp` as F64 / seconds, then ingest an ILP payload at second precision.
		let router = router_with_declared_temp(&dir).await;
		let payload = "weather,loc=a temp=1.5 100\nweather,loc=a temp=2.5 110\nweather,loc=a temp=3.5 120\n";
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?field=temp&precision=s", payload).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"], "temp");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 0);
		assert_eq!(body["min_ts"], 100);
		assert_eq!(body["max_ts"], 120);

		// Read it back through the JSON range endpoint.
		let router = router_with_declared_temp_reopened(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/temp/points?start=100&end=120").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let read: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(read["count"], 3);
		assert_eq!(read["points"][0]["value"], "1.5");
	}

	/// Declare `temp` (F64, seconds) in a fresh store under `dir`.
	async fn router_with_declared_temp(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		store.declare("temp", &dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds)).await.expect("declares");
		app_with_state(AppState::new().with_store(store))
	}

	/// Reopen a router over the store dir for reading back ILP-sealed data.
	async fn router_with_declared_temp_reopened(dir: &TempDir) -> axum::Router {
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("reopens store"));
		app_with_state(AppState::new().with_store(store))
	}

	#[tokio::test]
	async fn ilp_ingest_into_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let (status, _body) = post_text(router, "/api/v1/storage/ghost/ilp?field=temp&precision=s", "weather temp=1 100\n").await;
		assert_eq!(status, StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn ilp_ingest_unknown_precision_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?field=temp&precision=fortnights", "weather temp=1 100\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("unknown precision"));
	}

	#[tokio::test]
	async fn ilp_ingest_missing_field_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		// The payload carries `humidity`, but we ask for `temp`: no usable points.
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?field=temp&precision=s", "weather humidity=50 100\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("no points carrying field"));
	}

	#[tokio::test]
	async fn ilp_ingest_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let (status, _body) = post_text(router, "/api/v1/storage/temp/ilp?field=temp", "weather temp=1 100\n").await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]
	async fn csv_ingest_round_trips_through_the_read_surface() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		// A header row plus three data rows, the exact shape the CSV export emits.
		let csv = "timestamp,value\n100,1.5\n110,2.5\n120,3.5\n";
		let (status, body) = post_text(router, "/api/v1/storage/price/csv", csv).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"], "price");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 0);
		assert_eq!(body["min_ts"], 100);
		assert_eq!(body["max_ts"], 120);

		// The read surface returns exactly what the CSV body carried.
		let router = router_with_declared_price_reopened(&dir).await;
		let response = router.oneshot(Request::builder().uri("/api/v1/storage/price/points?start=100&end=120").body(Body::empty()).unwrap()).await.unwrap();
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let read: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(read["count"], 3);
		assert_eq!(read["points"][1]["value"], "2.5");
	}

	#[tokio::test]
	async fn csv_ingest_accepts_a_headerless_body() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv", "100,1.5\n110,2.5\n").await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["row_count"], 2);
	}

	#[tokio::test]
	async fn csv_ingest_empty_value_field_seals_a_null_row() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv", "100,1.5\n110,\n120,3.5\n").await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 1);
	}

	#[tokio::test]
	async fn csv_ingest_rows_per_page_seals_a_paged_frame() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv?rows_per_page=2", "100,1.5\n110,2.5\n120,3.5\n130,4.5\n").await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		// Paged frames carry the paged format version, distinct from a single-block frame.
		assert_eq!(body["format_version"], dsp_physical_type::PAGED_SEGMENT_FORMAT_VERSION);
		assert_eq!(body["row_count"], 4);
	}

	#[tokio::test]
	async fn csv_ingest_line_without_comma_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv", "100 1.5\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("no comma separator"));
	}

	#[tokio::test]
	async fn csv_ingest_non_integer_timestamp_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv", "not-a-ts,1.5\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("is not an integer"));
	}

	#[tokio::test]
	async fn csv_ingest_empty_body_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv", "\n  \n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("no CSV rows"));
	}

	#[tokio::test]
	async fn csv_ingest_into_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let (status, _body) = post_text(router, "/api/v1/storage/ghost/csv", "100,1.5\n").await;
		assert_eq!(status, StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn csv_ingest_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let (status, _body) = post_text(router, "/api/v1/storage/price/csv", "100,1.5\n").await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}

	#[test]
	fn parse_csv_points_reads_header_nulls_and_values() {
		let parsed = super::parse_csv_points("timestamp,value\n100,1.5\n110,\n120,3.5\n").expect("parses");
		assert_eq!(parsed.timestamps, vec![100, 110, 120]);
		assert_eq!(parsed.values, vec![Some("1.5".parse().unwrap()), None, Some("3.5".parse().unwrap())]);
		assert!(parsed.any_null);
	}

	#[tokio::test]
	async fn ingest_increments_shared_ingest_metrics() {
		use crate::SharedMetrics;

		let dir = TempDir::new().unwrap();
		let metrics = SharedMetrics::default();
		// Build state carrying both an observable metrics handle and a declared store.
		let store = Arc::new(SegmentStore::open(dir.path()).await.unwrap());
		store.declare("price", &dsp_physical_type::AspectSchema::new(dsp_physical_type::PhysicalType::F64, "0".parse().unwrap(), dsp_physical_type::TimeUnit::Seconds)).await.unwrap();
		let router = app_with_state(AppState::with_metrics(metrics.clone()).with_store(store));

		// A good ingest of three rows.
		let body = serde_json::json!({ "points": [
			{ "timestamp": 1, "value": "1" },
			{ "timestamp": 2, "value": "2" },
			{ "timestamp": 3, "value": "3" },
		] });
		let (status, _body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::CREATED);

		let snap = metrics.snapshot();
		assert_eq!(snap.ingest.requests, 1);
		assert_eq!(snap.ingest.errors, 0);
		assert_eq!(snap.ingest.rows_sealed, 3);
		assert_eq!(snap.ingest.segments_sealed, 1);
		// The seal path fed the ingest latency histogram (one observation).
		assert_eq!(metrics.ingest_latency.snapshot().count, 1);
	}

	#[tokio::test]
	async fn failed_ingest_increments_error_metric() {
		use crate::SharedMetrics;

		let dir = TempDir::new().unwrap();
		let metrics = SharedMetrics::default();
		let store = Arc::new(SegmentStore::open(dir.path()).await.unwrap());
		// No aspect declared -> the ingest 404s and must count one request + one error.
		let router = app_with_state(AppState::with_metrics(metrics.clone()).with_store(store));
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1" }] });
		let (status, _body) = post_json(router, "/api/v1/storage/ghost/points", &body).await;
		assert_eq!(status, StatusCode::NOT_FOUND);
		let snap = metrics.snapshot();
		assert_eq!(snap.ingest.requests, 1);
		assert_eq!(snap.ingest.errors, 1);
		assert_eq!(snap.ingest.rows_sealed, 0);
	}

	#[test]
	fn epoch_in_unit_rescales_to_the_declared_unit() {
		use chrono::TimeZone;
		let instant = chrono::Utc.timestamp_opt(100, 0).single().expect("valid instant");
		assert_eq!(super::epoch_in_unit(instant, dsp_physical_type::TimeUnit::Seconds), Some(100));
		assert_eq!(super::epoch_in_unit(instant, dsp_physical_type::TimeUnit::Millis), Some(100_000));
		assert_eq!(super::epoch_in_unit(instant, dsp_physical_type::TimeUnit::Micros), Some(100_000_000));
		assert_eq!(super::epoch_in_unit(instant, dsp_physical_type::TimeUnit::Nanos), Some(100_000_000_000));
	}
}
