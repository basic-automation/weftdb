//! Getting points into the store: the JSON, CSV, line-protocol and Parquet
//! ingest endpoints, the ordering gate they share, and the seal that turns a
//! batch into one `.weftseg` frame.
//!
//! Every ingest path is the same three steps: parse the body into timestamps and
//! values, check the ordering the aspect's contract asks for, then
//! [`weftdb::SegmentStore::seal_declared`] the batch as one segment. That is the
//! only seal path the HTTP surface has, so an ingest cannot slip past the
//! encoding/tolerance constraint (hard constraint #4 — a value that cannot be
//! represented under the declared encoding within tolerance is rejected, never
//! silently downcast).
//!
//! The Parquet ingest lives here rather than in [`crate::storage`] so that every
//! way a batch can arrive shares one ordering gate and one seal.

use axum::{
	body::Bytes, extract::{Path, State}, http::StatusCode, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;
use weft_line_protocol::TimestampPrecision;
use weft_physical_type::{first_order_violation, AspectSchema, SegmentDescriptor, TimeUnit};

use crate::{
	error::{self, ApiError, StrictJson, StrictQuery}, state::AppState
};

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
	/// The `.weftseg` frame format version sealed (5 single-block, 6 paged).
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

/// Enforce monotonic non-decreasing timestamps when a request opts in
/// (`require_sorted`) — the API surface of the Phase-4.2 order enforcement
/// ([`weft_physical_type::first_order_violation`] / `AspectSchema::seal_sorted`).
///
/// A client ingesting an append-only stream can demand the server reject an
/// out-of-order batch (which would otherwise seal an unsortable segment that forces
/// linear scans on read) rather than silently store it. Returns
/// [`ApiError::bad_request`] naming the first backwards row; a `false` flag (the
/// default) is a no-op so the permissive path is unchanged.
fn enforce_order_if_required(require_sorted: bool, timestamps: &[i64]) -> Result<(), ApiError> {
	if require_sorted {
		if let Some((index, previous, current)) = first_order_violation(timestamps) {
			return Err(ApiError::bad_request(format!("out-of-order timestamp at row {index}: {current} < previous {previous} (require_sorted was set)")));
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
/// [`ApiError::unconfigured`] (no store), [`ApiError::aspect_not_found`] (aspect
/// undeclared), [`ApiError::bad_request`] (empty batch, an unparseable value, or
/// values unrepresentable under the declared encoding/tolerance), and
/// [`ApiError::internal`] on a filesystem/control-plane failure.
pub async fn ingest_points(State(state): State<AppState>, Path(aspect): Path<String>, StrictJson(request): StrictJson<IngestRequest>) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
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
async fn ingest_points_inner(store: &weftdb::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, request: IngestRequest) -> Result<Response, ApiError> {
	if request.points.is_empty() {
		return Err(ApiError::bad_request("no points to ingest (`points` is empty)".to_string()));
	}
	// An undeclared aspect is a clean 404 (its encoding is unknown) rather than the
	// seal's generic error; fetching the schema here also lets the nullable/paged
	// seal variants take it directly. A store that refuses the name, or a root this binary will
	// not open, is classified by its type rather than reported as a `500`.
	let Some(schema) = store.schema_for(aspect).await.map_err(|err| error::classify_read(&err))? else {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
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
					let parsed: BigDecimal = weft_physical_type::parse_decimal(text).map_err(|e| error::decimal_error(e, ApiError::bad_request(format!("value {text:?} (at timestamp {}) is not a decimal", point.timestamp))))?;
					values.push(Some(parsed));
				}
			}
		}
	}

	enforce_order_if_required(request.require_sorted, &timestamps)?;
	// `storage.ingest.seal` child span (roadmap Phase 3): the typed-column encode +
	// `.weftseg` write + control-plane index update — the write stage timed apart from
	// value parsing.
	let descriptor = seal_batch(store, aspect, &schema, &timestamps, &values, any_null, request.rows_per_page).instrument(tracing::info_span!("storage.ingest.seal", point_count, any_null, format = "json")).await.map_err(|err| error::classify_ingest(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse { aspect: aspect.to_string(), segment_id: descriptor.id, format_version: descriptor.format_version, row_count: descriptor.row_count, null_count: descriptor.null_count, byte_len: descriptor.byte_len, min_ts: descriptor.min_ts, max_ts: descriptor.max_ts, time_sorted: descriptor.time_sorted };
	Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// Dispatch the right seal path for the batch: nullable vs dense × paged vs
/// single-block. Kept as a free async fn taking `&SegmentStore` so the handler can
/// drop its store handle before building the response.
async fn seal_batch(store: &weftdb::SegmentStore, aspect: &str, schema: &AspectSchema, timestamps: &[i64], values: &[Option<BigDecimal>], any_null: bool, rows_per_page: Option<usize>) -> anyhow::Result<SegmentDescriptor> {
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
/// [`ApiError::bad_request`] for a line without a comma, a non-integer
/// timestamp, an unparseable value, or an empty body (no data rows).
fn parse_csv_points(body: &str) -> Result<ParsedCsv, ApiError> {
	let mut timestamps = Vec::new();
	let mut values = Vec::new();
	let mut any_null = false;
	for (index, raw) in body.lines().enumerate() {
		let line = raw.trim();
		if line.is_empty() {
			continue;
		}
		let (ts_text, value_text) = line.split_once(',').ok_or_else(|| ApiError::bad_request(format!("CSV line {} has no comma separator: {raw:?}", index + 1)))?;
		let ts_text = ts_text.trim();
		let value_text = value_text.trim();
		// Skip an optional leading header row (`timestamp,value`).
		if timestamps.is_empty() && ts_text.eq_ignore_ascii_case("timestamp") {
			continue;
		}
		let timestamp: i64 = ts_text.parse().map_err(|_| ApiError::bad_request(format!("CSV line {}: timestamp {ts_text:?} is not an integer", index + 1)))?;
		if value_text.is_empty() {
			any_null = true;
			values.push(None);
		} else {
			let parsed: BigDecimal = weft_physical_type::parse_decimal(value_text).map_err(|e| error::decimal_error(e, ApiError::bad_request(format!("CSV line {}: value {value_text:?} is not a decimal", index + 1))))?;
			values.push(Some(parsed));
		}
		timestamps.push(timestamp);
	}
	if timestamps.is_empty() {
		return Err(ApiError::bad_request("no CSV rows to ingest".to_string()));
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
/// [`ApiError::unconfigured`] (no store), [`ApiError::aspect_not_found`] (aspect
/// undeclared), [`ApiError::bad_request`] (malformed CSV, empty body, or values
/// unrepresentable under the declared encoding/tolerance), and
/// [`ApiError::internal`] on a filesystem/control-plane failure.
pub async fn ingest_csv(State(state): State<AppState>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<CsvIngestParams>, body: String) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
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
/// any failure path uniformly. Shares [`seal_batch`] and `error::classify_ingest` with
/// the JSON ingest path, so a CSV-sealed batch is byte-identical to a JSON-sealed one.
async fn ingest_csv_inner(store: &weftdb::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, rows_per_page: Option<usize>, require_sorted: bool, body: &str) -> Result<Response, ApiError> {
	let Some(schema) = store.schema_for(aspect).await.map_err(|err| error::classify_read(&err))? else {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	// `storage.ingest.parse` decode span (roadmap Phase 3): the CSV text → typed
	// `(timestamp, value)` columns, timed apart from the seal.
	let ParsedCsv { timestamps, values, any_null } = tracing::info_span!("storage.ingest.parse", format = "csv").in_scope(|| parse_csv_points(body))?;
	enforce_order_if_required(require_sorted, &timestamps)?;
	let point_count = timestamps.len();
	let descriptor = seal_batch(store, aspect, &schema, &timestamps, &values, any_null, rows_per_page).instrument(tracing::info_span!("storage.ingest.seal", point_count, any_null, format = "csv")).await.map_err(|err| error::classify_ingest(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse { aspect: aspect.to_string(), segment_id: descriptor.id, format_version: descriptor.format_version, row_count: descriptor.row_count, null_count: descriptor.null_count, byte_len: descriptor.byte_len, min_ts: descriptor.min_ts, max_ts: descriptor.max_ts, time_sorted: descriptor.time_sorted };
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
/// `weft-line-protocol` crate, so the storage ingest path and the interpolation ILP
/// path accept the exact same dialect. Each point's absolute instant is rescaled to
/// the aspect's declared [`TimeUnit`] before sealing. ILP fields are always present,
/// so this is a dense seal.
///
/// # Errors
///
/// [`ApiError::unconfigured`] (no store), [`ApiError::aspect_not_found`] (aspect
/// undeclared), [`ApiError::bad_request`] (unknown precision token, malformed
/// payload, no points carrying the field, a timestamp outside the declared unit's
/// range, or values unrepresentable under the declared encoding/tolerance), and
/// [`ApiError::internal`] on a filesystem/control-plane failure.
pub async fn ingest_ilp(State(state): State<AppState>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<IlpIngestParams>, body: String) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
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
async fn ingest_ilp_inner(store: &weftdb::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, params: &IlpIngestParams, body: &str) -> Result<Response, ApiError> {
	let precision = parse_ilp_precision(params.precision.as_deref()).map_err(ApiError::bad_request)?;
	let Some(schema) = store.schema_for(aspect).await.map_err(|err| error::classify_read(&err))? else {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	// `storage.ingest.parse` decode span (roadmap Phase 3): the line-protocol text →
	// typed points, timed apart from the timestamp normalization and the seal.
	let points = tracing::info_span!("storage.ingest.parse", format = "ilp", field = %params.field).in_scope(|| weft_line_protocol::parse_points(body, &params.field, precision)).map_err(|err| ApiError::bad_request(err.to_string()))?;
	if points.is_empty() {
		return Err(ApiError::bad_request(format!("no points carrying field `{}` with a timestamp in the payload", params.field)));
	}

	// `storage.ingest.normalize` span (roadmap Phase 3): rescale each wire epoch to the
	// aspect's declared `TimeUnit` — the timestamp-normalization stage, distinct from
	// the wire decode above.
	let (timestamps, values) = {
		let _normalize = tracing::info_span!("storage.ingest.normalize", point_count = points.len(), unit = %schema.timestamp_unit.name()).entered();
		let mut timestamps = Vec::with_capacity(points.len());
		let mut values = Vec::with_capacity(points.len());
		for point in points {
			let epoch = epoch_in_unit(point.timestamp, schema.timestamp_unit).ok_or_else(|| ApiError::bad_request(format!("timestamp {} is outside the range of the declared `{}` unit", point.timestamp, schema.timestamp_unit.name())))?;
			// The one decimal domain (ROB-6) at this parse site too: an ILP
			// float enters as its exact `f64` expansion, whose plain form a
			// subnormal can push past the bound — refused here with the
			// domain's code rather than later, as a seal failure.
			weft_physical_type::check_decimal_domain(&point.value).map_err(|e| error::decimal_out_of_range_refusal(e.digits))?;
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
	.map_err(|err| error::classify_ingest(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse { aspect: aspect.to_string(), segment_id: descriptor.id, format_version: descriptor.format_version, row_count: descriptor.row_count, null_count: descriptor.null_count, byte_len: descriptor.byte_len, min_ts: descriptor.min_ts, max_ts: descriptor.max_ts, time_sorted: descriptor.time_sorted };
	Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// Query parameters for the Parquet ingest endpoint: an optional page height.
#[derive(Debug, Clone, Deserialize)]
pub struct ParquetIngestParams {
	/// Optional page height — seal a paged segment of this many rows per page.
	#[serde(default)]
	pub rows_per_page: Option<usize>,
	/// When `true`, reject an out-of-order batch (`400`) instead of sealing it — the
	/// Parquet counterpart of the JSON/CSV/ILP `require_sorted` flag.
	#[serde(default)]
	pub require_sorted: bool,
}

/// Handle `POST /api/v1/storage/{aspect}/parquet?rows_per_page`.
///
/// The write-side counterpart of the Parquet range export and the Parquet sibling
/// of the JSON / ILP ingest endpoints: ingests an Apache Parquet file (the request
/// body, `application/vnd.apache.parquet`) into `aspect`'s declared schema, sealing
/// one new segment. The decode + seal lives in `weft-arrow-store` (so the heavy
/// `arrow-*` tree stays off the lean core); the no-silent-downcast guarantee holds
/// — a value unrepresentable under the declared encoding/tolerance is rejected
/// `400`, never downcast. Returns `201 Created` with the sealed segment's descriptor.
///
/// # Errors
///
/// [`ApiError::unconfigured`] (no store), [`ApiError::aspect_not_found`] (aspect
/// undeclared), [`ApiError::bad_request`] (empty/malformed Parquet body or values
/// unrepresentable under the declared encoding/tolerance), and
/// [`ApiError::internal`] on a filesystem/control-plane failure.
pub async fn ingest_parquet(State(state): State<AppState>, Path(aspect): Path<String>, StrictQuery(params): StrictQuery<ParquetIngestParams>, body: Bytes) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
	drop(state);
	let start = std::time::Instant::now();
	let result = ingest_parquet_inner(&store, &metrics, &aspect, params.rows_per_page, params.require_sorted, &body).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// The body of [`ingest_parquet`], split out so the handler records an error
/// metric for any failure path uniformly.
async fn ingest_parquet_inner(store: &weftdb::SegmentStore, metrics: &crate::metrics::SharedMetrics, aspect: &str, rows_per_page: Option<usize>, require_sorted: bool, body: &Bytes) -> Result<Response, ApiError> {
	if body.is_empty() {
		return Err(ApiError::bad_request("empty Parquet body".to_string()));
	}
	// `storage.ingest.parquet` stage span (roadmap Phase 3 — the Arrow-decode-on-ingest
	// analogue of the JSON/CSV/ILP `storage.ingest.parse`+`seal` spans). The arrow-store
	// ingest is atomic (Parquet decode → typed-column seal in one call) and the leaf
	// `weft-arrow-store` stays tracing-free by design, so decode+seal ride one span here,
	// carrying the request's byte length and the sort-guard flag.
	let descriptor = weft_arrow_store::ingest_parquet_into_aspect(store, aspect, body, rows_per_page, require_sorted).instrument(tracing::info_span!("storage.ingest.parquet", %aspect, byte_len = body.len(), require_sorted, format = "parquet")).await.map_err(|err| error::classify_ingest(&err))?;
	metrics.record_ingest_seal(u64::try_from(descriptor.row_count).unwrap_or(u64::MAX));
	let response = IngestResponse { aspect: aspect.to_string(), segment_id: descriptor.id, format_version: descriptor.format_version, row_count: descriptor.row_count, null_count: descriptor.null_count, byte_len: descriptor.byte_len, min_ts: descriptor.min_ts, max_ts: descriptor.max_ts, time_sorted: descriptor.time_sorted };
	Ok((StatusCode::CREATED, Json(response)).into_response())
}
#[cfg(test)]

mod tests {

	use std::sync::Arc;

	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tempfile::TempDir;
	use tower::ServiceExt;
	use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
	use weftdb::SegmentStore;

	use crate::{app_with_state, test_support::*, AppState};

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
		assert_eq!(body["format_version"], weft_physical_type::PAGED_SEGMENT_FORMAT_VERSION);
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
	async fn ingest_out_of_domain_value_is_decimal_out_of_range() {
		// ROB-6: a value whose plain form is over 1,024 digits is refused at
		// the parse site with the domain's own code, before any seal runs.
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let over = format!("1{}", "0".repeat(1024));
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": over }] });
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert_eq!(body["code"], "decimal_out_of_range", "body: {body}");
		assert_eq!(body["limit"], 1024, "body: {body}");
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
		store.declare("counts", &weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::ScaledI64 { scale: 0 }, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds)).await.unwrap();
		let router = app_with_state(AppState::new().with_store(store));
		let body = serde_json::json!({ "points": [{ "timestamp": 1, "value": "1.5" }] });
		let (status, body) = post_json(router, "/api/v1/storage/counts/points", &body).await;
		// The request was well-formed and the batch was readable; what cannot be represented is
		// its content, so the §4 table answers `422 value_not_representable` rather than the
		// `400` every seal failure used to share — and it says which way the encoding would fail.
		assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
		assert_eq!(body["code"], "value_not_representable", "body: {body}");
		assert_eq!(body["details"]["max_abs_error"], "0.5", "the loss the declared encoding would take: {body}");
		assert_eq!(body["details"]["tolerance"], "0", "the tolerance the schema declares: {body}");
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
		// The `weft-line-protocol` parser returns points sorted by timestamp, so an
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
		assert_eq!(body["format_version"], weft_physical_type::PAGED_SEGMENT_FORMAT_VERSION);
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
		store.declare("price", &weft_physical_type::AspectSchema::new(weft_physical_type::PhysicalType::F64, "0".parse().unwrap(), weft_physical_type::TimeUnit::Seconds)).await.unwrap();
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
		assert_eq!(super::epoch_in_unit(instant, weft_physical_type::TimeUnit::Seconds), Some(100));
		assert_eq!(super::epoch_in_unit(instant, weft_physical_type::TimeUnit::Millis), Some(100_000));
		assert_eq!(super::epoch_in_unit(instant, weft_physical_type::TimeUnit::Micros), Some(100_000_000));
		assert_eq!(super::epoch_in_unit(instant, weft_physical_type::TimeUnit::Nanos), Some(100_000_000_000));
	}
	#[tokio::test]

	async fn parquet_ingest_require_sorted_rejects_out_of_order() {
		use weft_arrow::{columns_to_record_batch_typed, write_parquet};

		let dir = TempDir::new().expect("temp dir");

		let store = SegmentStore::open(dir.path()).await.expect("opens store");

		store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");

		let router = app_with_state(AppState::new().with_store(Arc::new(store)));

		// An out-of-order Parquet batch (row 1: 90 < 100).

		let batch = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, &[100_i64, 90, 120], &[Some(bd("1")), Some(bd("2")), Some(bd("3"))]);

		let bytes = write_parquet(std::slice::from_ref(&batch)).expect("writes parquet");

		let post = |uri: &'static str| {
			let router = router.clone();

			let body = bytes.clone();

			async move { router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/vnd.apache.parquet").body(Body::from(body)).unwrap()).await.unwrap() }
		};

		// require_sorted -> 400 with the offending row.

		let rejected = post("/api/v1/storage/price/parquet?require_sorted=true").await;

		assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

		let msg = axum::body::to_bytes(rejected.into_body(), usize::MAX).await.unwrap();

		let msg: serde_json::Value = serde_json::from_slice(&msg).unwrap();

		assert!(msg["error"].as_str().unwrap().contains("out-of-order timestamp"), "body: {msg}");

		// Default (no flag) -> 201.

		let accepted = post("/api/v1/storage/price/parquet").await;

		assert_eq!(accepted.status(), StatusCode::CREATED);
	}

	#[tokio::test]

	async fn parquet_ingest_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());

		let (status, _json) = post_parquet(router, "/api/v1/storage/price/parquet", price_parquet_body(&[1], &[Some(bd("1"))])).await;

		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	}

	#[tokio::test]

	async fn parquet_ingest_empty_body_is_bad_request() {
		let dir = TempDir::new().unwrap();

		let router = router_with_declared_empty_price(&dir).await;

		let (status, _json) = post_parquet(router, "/api/v1/storage/price/parquet", Vec::new()).await;

		assert_eq!(status, StatusCode::BAD_REQUEST);
	}

	#[tokio::test]

	async fn parquet_ingest_rejects_a_garbage_body() {
		let dir = TempDir::new().unwrap();

		let router = router_with_declared_empty_price(&dir).await;

		let (status, _json) = post_parquet(router, "/api/v1/storage/price/parquet", b"not a parquet file".to_vec()).await;

		assert_eq!(status, StatusCode::BAD_REQUEST);
	}

	#[tokio::test]

	async fn parquet_ingest_into_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();

		let router = router_with_declared_empty_price(&dir).await;

		let body = price_parquet_body(&[1], &[Some(bd("1"))]);

		let (status, _json) = post_parquet(router, "/api/v1/storage/never_declared/parquet", body).await;

		assert_eq!(status, StatusCode::NOT_FOUND);
	}

	#[tokio::test]

	async fn parquet_ingest_seals_a_batch_the_read_surface_returns() {
		let dir = TempDir::new().unwrap();

		let router = router_with_declared_empty_price(&dir).await;

		let body = price_parquet_body(&[100, 110, 120], &[Some(bd("1.5")), Some(bd("2.5")), Some(bd("3.5"))]);

		let (status, json) = post_parquet(router, "/api/v1/storage/price/parquet", body).await;

		assert_eq!(status, StatusCode::CREATED, "body: {json}");

		assert_eq!(json["aspect"], "price");

		assert_eq!(json["row_count"], 3);

		assert_eq!(json["null_count"], 0);

		assert_eq!(json["min_ts"], 100);

		assert_eq!(json["max_ts"], 120);

		// Reopen and read back through the JSON points surface.

		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("reopens"));

		let router = app_with_state(AppState::new().with_store(store));

		let (status, read) = get_json(router, "/api/v1/storage/price/points?start=100&end=120").await;

		assert_eq!(status, StatusCode::OK);

		assert_eq!(read["count"], 3);

		assert_eq!(read["points"][1]["value"], "2.5");
	}

	#[tokio::test]
	async fn parquet_ingest_feeds_the_ingest_latency_histogram() {
		use crate::SharedMetrics;

		let dir = TempDir::new().unwrap();
		let metrics = SharedMetrics::default();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		store.declare("price", &AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds)).await.expect("declares");
		let router = app_with_state(AppState::with_metrics(metrics.clone()).with_store(store));

		let body = price_parquet_body(&[100, 110, 120], &[Some(bd("1.5")), Some(bd("2.5")), Some(bd("3.5"))]);
		let (status, json) = post_parquet(router, "/api/v1/storage/price/parquet", body).await;
		assert_eq!(status, StatusCode::CREATED, "body: {json}");

		// The Parquet leg already counted requests, rows and segments like the other
		// three formats; it has to feed the same latency histogram too, otherwise
		// `GET /debug/profile/current` reports ingest p50/p95/p99 measured over three
		// formats while its `requests`/`rows_sealed` counters cover all four.
		let snap = metrics.snapshot();
		assert_eq!(snap.ingest.requests, 1);
		assert_eq!(snap.ingest.rows_sealed, 3);
		assert_eq!(snap.ingest.segments_sealed, 1);
		assert_eq!(metrics.ingest_latency.snapshot().count, 1, "the Parquet ingest leg must observe ingest latency as JSON / CSV / ILP do");
	}
}
