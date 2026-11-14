//! OpenAPI 3.1 document generation from the route registry (FRE-9a).
//!
//! The document is built from the same [`RouteTable`] the router serves, so
//! every path, method, scope, admission class and tier that the wire presents
//! is the one the code registered. Adding a route changes the generated
//! document; a test asserts the committed spec matches it (the drift gate).
//!
//! FRE-9b (W11) annotates the document into the **contract candidate**: every
//! operation carries its description, its query parameters (including the
//! dynamic `tag.*` selector family TAG-10 left undescribed), its request body
//! and its success response, with a component schema for every request and
//! response body — the wire format `docs/stability.md` defines as "routes,
//! request bodies, query parameters, and response schemas documented in the
//! committed OpenAPI spec". The annotations are data in this module, read off
//! the handlers' own extractor types, so the drift gate still pins the whole
//! document to what the code serves; a test asserts no operation is left
//! unannotated. The beta tag freezes this document as the contract FRE-18's
//! breaking-change check compares against at rc.1.
//!
//! ## Subcommand
//!
//! ```text
//! weft-server openapi [--out PATH]
//! ```
//!
//! With no `--out`, or `--out -`, the document is written to stdout. With
//! `--out <path>`, it is written to that file. The command refuses a directory
//! or an unwritable path with exit 78.

use std::collections::BTreeMap;

use axum::http::Method;
use serde_json::{json, Map, Value};

use crate::routes::{RouteSpec, RouteTable, Scope};

/// The first argv word that selects this command instead of serving.
pub const SUBCOMMAND: &str = "openapi";

/// The HTTP status codes the server's error table can produce, derived from
/// `Code::status` in `error.rs`.
pub const ERROR_STATUSES: &[u16] = &[400, 401, 403, 404, 405, 408, 409, 410, 413, 415, 422, 500, 503, 504, 507];

const fn error_status_description(status: u16) -> &'static str {
	match status {
		400 => "Bad Request — the request body or query is malformed",
		401 => "Unauthenticated — the request carries no `Authorization` header, or the token is invalid",
		403 => "Forbidden — the caller's key does not hold the scope this route requires",
		404 => "Not Found — no route matches the path, or the named resource does not exist",
		405 => "Method Not Allowed — the path exists but not under this HTTP method",
		408 => "Request Timeout — the client did not send the body within the server's configured deadline",
		409 => "Conflict — the request would create a duplicate, a schema mismatch, or the resource is busy",
		410 => "Gone — the cursor the request refers to is stale and cannot be advanced",
		413 => "Payload Too Large — the request body exceeds the server's configured limit",
		415 => "Unsupported Media Type — the `Content-Type` is not one the route accepts",
		422 => "Unprocessable Entity — the result exceeds the server's configured output limit",
		500 => "Internal Server Error — the server could not complete the request",
		503 => "Service Unavailable — a subsystem is not configured, not ready, overloaded, or poisoned",
		504 => "Gateway Timeout — the operation did not finish within its configured deadline",
		507 => "Insufficient Storage — the filesystem cannot safely accept the write",
		_ => "unexpected error",
	}
}

// ── FRE-9b annotations ───────────────────────────────────────────────────
//
// One [`OperationAnnotation`] per `operation_id`, read off the handler's own
// extractor types (`StrictQuery`/`SelectorQuery`/`TaggedQuery` for the query
// parameters, `StrictJson`/`StrictText`/`StrictBody` for the request body,
// the handler's return type for the success response). The annotation table
// is exhaustive — [`annotation_for`] returns `None` for an unannotated
// operation and a test fails the build on that — so a route added without its
// contract fails here instead of shipping an undescribed operation.
//
// Media types are the ones the handlers themselves send or accept
// (`text/csv; charset=utf-8`, `application/vnd.apache.arrow.stream`,
// `application/vnd.apache.parquet`, `text/plain; version=0.0.4` for
// Prometheus). A success status of `201` is what the handler answers
// (`declare_aspect`, and ingest except on an idempotent replay, which
// answers `200` with the same body).

/// The media types the wire uses, named once so an annotation cannot typo one.
const JSON: &str = "application/json";
const CSV: &str = "text/csv; charset=utf-8";
const ARROW: &str = "application/vnd.apache.arrow.stream";
const PARQUET: &str = "application/vnd.apache.parquet";
const TEXT: &str = "text/plain";
const PROMETHEUS: &str = "text/plain; version=0.0.4";

/// One query parameter of an operation, as the handler's query struct declares
/// it: `name` is the wire name, `required` whether the struct field has no
/// `#[serde(default)]`, `ty` the JSON type, `description` the field's own doc
/// comment in one sentence.
struct QueryParam {
	name: &'static str,
	required: bool,
	ty: &'static str,
	description: &'static str,
}

const fn query(name: &'static str, required: bool, ty: &'static str, description: &'static str) -> QueryParam {
	QueryParam { name, required, ty, description }
}

/// The `tag.*` selector family (TAG-10) and its `tag_match` switch, carried by
/// every `SelectorQuery` read and by the CSV/Parquet ingest routes (TAG-11).
/// `OpenAPI` cannot enumerate dynamic names, so the family is documented as one
/// `tag.*` parameter whose description states the grammar; `tag_match` is the
/// real parameter name.
const SELECTOR_PARAMS: &[QueryParam] = &[query("tag.*", false, "string", "A `tag.<key>=<value>` matcher selecting the series to read (at most 16 matchers; a repeated key is `400 invalid_tag`). No `tag.*` reads the untagged series 0."), query("tag_match", false, "string", "`exact` (default) or `subset`: whether the series' tags must equal the matchers or merely contain them.")];

/// `TimeRangeParams` (`storage.rs`): the inclusive `[start, end]` window.
const TIME_RANGE_QUERY: &[QueryParam] = &[query("start", true, "integer", "Inclusive window start (epoch integer in the aspect's declared timestamp unit)."), query("end", true, "integer", "Inclusive window end (epoch integer in the aspect's declared timestamp unit).")];

/// `ValueRangeParams`: the inclusive `[lo, hi]` value band as decimal text.
const VALUE_RANGE_QUERY: &[QueryParam] = &[query("lo", true, "string", "Inclusive lower value bound (decimal text)."), query("hi", true, "string", "Inclusive upper value bound (decimal text).")];

/// `PointsRangeParams`: the window plus B-rest pagination.
const POINTS_RANGE_QUERY: &[QueryParam] = &[query("start", true, "integer", "Inclusive window start (epoch integer in the aspect's declared timestamp unit)."), query("end", true, "integer", "Inclusive window end (epoch integer in the aspect's declared timestamp unit)."), query("offset", false, "integer", "Skip this many leading rows of the window before returning (default 0)."), query("limit", false, "integer", "Return at most this many rows (default: this server's page size; over `MAX_PAGE_ROWS` is `422 result_too_large`)."), query("take", false, "integer", "Declarative alias for `limit` (the page size). `limit` takes precedence if both are supplied."), query("page", false, "integer", "1-based page number. With a page size (`limit`/`take`) it derives the offset and supersedes `offset`."), query("cursor", false, "string", "Opaque forward-iteration cursor (from a prior response's `next_cursor`). Supersedes `offset`/`page`; malformed → `400`.")];

/// `ValuePointsParams`: the value band plus B-rest pagination.
const VALUE_POINTS_QUERY: &[QueryParam] = &[query("lo", true, "string", "Inclusive lower value bound (decimal text)."), query("hi", true, "string", "Inclusive upper value bound (decimal text)."), query("offset", false, "integer", "Skip this many leading rows before returning (default 0)."), query("limit", false, "integer", "Return at most this many rows (default: this server's page size; over `MAX_PAGE_ROWS` is `422 result_too_large`)."), query("take", false, "integer", "Declarative alias for `limit` (the page size). `limit` wins if both are given."), query("page", false, "integer", "1-based page number; it derives the offset from the resolved page size."), query("cursor", false, "string", "Opaque forward-iteration cursor (from a prior response's `next_cursor`); supersedes `offset`/`page`, malformed → `400`.")];

/// `PointParams`: the single instant to look up.
const POINT_QUERY: &[QueryParam] = &[query("t", true, "integer", "The instant to look up (epoch integer in the aspect's declared unit).")];

/// `MultiPointParams`: the comma-separated instants to look up.
const MULTI_POINT_QUERY: &[QueryParam] = &[query("t", true, "string", "Comma-separated epoch integers in the aspect's declared unit (e.g. `1000,2000,3000`).")];

/// `StorageDownsampleParams`: the stored-range downsample's window and grid.
const STORAGE_DOWNSAMPLE_QUERY: &[QueryParam] = &[query("start", false, "integer", "Inclusive window start (epoch integer in the aspect's declared unit). Defaults to the whole stored history when omitted."), query("end", false, "integer", "Inclusive window end (epoch integer in the aspect's declared unit). Defaults to the whole stored history when omitted."), query("resolution", false, "string", "Resolution token (`seconds`..`years`) or bucket width (`5s`, `15m`); defaults to minutes."), query("agg", false, "string", "Comma-separated reductions (e.g. `min,max,sketch_p99`); defaults to `min,max,avg`."), query("fill", false, "string", "Gap fill method (`null`, `prev`, `linear`, `quadratic`, `cubic` or a constant): every grid step between the bounds is emitted, the empty ones with `count == 0`.")];

/// `StoredInterpolateParams`: the stored-range interpolation's window and grid.
const STORED_INTERPOLATE_QUERY: &[QueryParam] = &[query("start", true, "integer", "Inclusive grid start (epoch integer in the aspect's declared unit)."), query("end", true, "integer", "Inclusive grid end (epoch integer in the aspect's declared unit)."), query("resolution", false, "string", "Grid resolution token (`nanoseconds`..`years`); defaults to `minutes`."), query("spline", false, "string", "Spline token (`linear`/`quadratic`/`cubic`); defaults to `cubic`."), query("margin", false, "integer", "How far outside `[start, end]` stored samples are read as knots, in the aspect's unit; at least 0. Defaults to `end − start`.")];

/// `IlpParams`: the ILP interpolation endpoint's query half.
const ILP_QUERY: &[QueryParam] = &[query("field", true, "string", "Numeric field to project each line-protocol record onto."), query("precision", false, "string", "Timestamp precision token (`ns`/`us`/`ms`/`s`); defaults to nanoseconds (ILP's own default)."), query("spline", false, "string", "Spline token (`linear`/`quadratic`/`cubic`); defaults to cubic."), query("interpolation", false, "string", "Alias for `spline` — accepts the same tokens. `spline` takes precedence when both are supplied."), query("resolution", false, "string", "Resolution token (`seconds`..`years`); defaults to minutes.")];

/// `DownsampleIlpParams`: the ILP downsample endpoint's query half.
const DOWNSAMPLE_ILP_QUERY: &[QueryParam] = &[query("field", true, "string", "Numeric field to project each line-protocol record onto."), query("precision", false, "string", "Timestamp precision token (`ns`/`us`/`ms`/`s`); defaults to nanoseconds."), query("resolution", false, "string", "Resolution token (`seconds`..`years`) or bucket width (`5s`, `15m`); defaults to minutes."), query("agg", false, "string", "Comma-separated reductions (e.g. `min,max,avg`); defaults to `min,max,avg`."), query("fill", false, "string", "Gap fill method (`null`, `prev`, `linear`, `quadratic`, `cubic` or a constant).")];

/// `CsvIngestParams`: the CSV ingest route's query half.
const CSV_INGEST_QUERY: &[QueryParam] = &[query("rows_per_page", false, "integer", "Optional page height — seal a paged segment of this many rows per page."), query("require_sorted", false, "boolean", "When `true`, reject an out-of-order batch (`400`) instead of sealing it.")];

/// `IlpIngestParams`: the ILP ingest route's query half.
const ILP_INGEST_QUERY: &[QueryParam] = &[query("field", true, "string", "The line-protocol field key whose numeric values are sealed (one aspect == one field)."), query("measurement", true, "string", "The line-protocol measurement whose records are sealed (required). Records of any other measurement are skipped."), query("precision", false, "string", "The wire timestamp precision token (`ns`/`us`/`ms`/`s`, default `ns`) — how the integer timestamps in the payload are interpreted."), query("rows_per_page", false, "integer", "Optional page height — seal a paged segment of this many rows per page."), query("require_sorted", false, "boolean", "When `true`, reject an out-of-order batch (`400`) instead of sealing it.")];

/// `ParquetIngestParams`: the Parquet ingest route's query half.
const PARQUET_INGEST_QUERY: &[QueryParam] = &[query("rows_per_page", false, "integer", "Optional page height — seal a paged segment of this many rows per page."), query("require_sorted", false, "boolean", "When `true`, reject an out-of-order batch (`400`) instead of sealing it — the Parquet counterpart of the JSON/CSV/ILP `require_sorted` flag.")];

/// `ReconcileParams`: the reconcile endpoints' query half.
const RECONCILE_QUERY: &[QueryParam] = &[query("threshold", false, "integer", "Order-health trigger: the pass runs only if the aspect's `unsorted_segments` backlog is at or above this many (0 clamps to 1). Absent, the pass runs unconditionally."), query("hot_cold", false, "boolean", "When `true`, reconcile in hot/cold mode: rewrite cold out-of-order segments regardless of the threshold, deferring only the hot tail."), query("overlaps", false, "boolean", "When `true`, run the cross-segment overlap merge: collapse each group of time-overlapping segments into one (newer-wins). Takes precedence over `hot_cold`/`threshold`."), query("split_min_bytes", false, "integer", "Split-not-rewrite floor in bytes for the overlap merge, paired with `overlaps=true`. Absent, the merge uses the default 50 MiB floor.")];

/// `SquashParams`: the squash endpoint's query half.
const SQUASH_QUERY: &[QueryParam] = &[query("max_segments", false, "integer", "Segment-count cap: squash only when the aspect has more than this many segments (0 clamps to 1). Absent, the squash runs unconditionally.")];

/// `CompactParams`: the compact endpoint's query half.
const COMPACT_QUERY: &[QueryParam] = &[query("target_rows", true, "integer", "The target segment size in rows (required; absent → `400`). Segments coalesce toward ~this many rows per segment.")];

/// `QuarantineParams`: the quarantine listing's query half.
const QUARANTINE_QUERY: &[QueryParam] = &[query("aspect", false, "string", "Restrict the listing to one aspect when present; list every aspect when absent.")];

/// `RestoreDrillParams`: the restore drill's query half.
const RESTORE_DRILL_QUERY: &[QueryParam] = &[query("label", true, "string", "The backup subdirectory to rehearse restoring, under the backup root, in the label grammar (`[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not ending with `.`).")];

/// `RestoreIntoParams`: the restore-into endpoint's query half.
const RESTORE_INTO_QUERY: &[QueryParam] = &[query("label", true, "string", "The backup subdirectory to restore, under the backup root, in the label grammar (`[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not ending with `.`)."), query("name", true, "string", "A single safe path segment naming the destination under the restore base directory. Non-empty, ≤ 200 bytes, `[A-Za-z0-9._-]+`, no `/`, no `\\`, no `..`, no leading `.`.")];

/// `BackupParams`: the backup endpoint's query half.
const BACKUP_QUERY: &[QueryParam] = &[query("label", false, "string", "A subdirectory name for this snapshot under the backup root, in the label grammar. Absent → a `manual-<unix_millis>` name is generated."), query("verify", false, "string", "`source` (default) cross-checks the copy against a fresh read of the live control plane — the strongest check, for a quiescent store; `snapshot` verifies the copy on its own terms, for an online backup taken while ingest continues.")];

/// No query parameters: the operation takes none.
const NO_QUERY: &[QueryParam] = &[];

/// A request body: its media type, its schema (a component name, or a plain
/// JSON type for a text/binary body), and what the body carries.
struct RequestBody {
	media_type: &'static str,
	/// Component schema name, when the body is a JSON object the spec models.
	schema: Option<&'static str>,
	/// Plain JSON type (`string`) for a text/binary body, when there is no
	/// component schema.
	inline_type: Option<&'static str>,
	description: &'static str,
}

/// One success response: its status, media type, component schema (when the
/// body is a modelled JSON object), and what it carries.
struct SuccessResponse {
	status: u16,
	media_type: &'static str,
	/// Component schema name, when the body is a modelled JSON object.
	schema: Option<&'static str>,
	description: &'static str,
}

const fn json_success_status(status: u16, schema: &'static str, description: &'static str) -> SuccessResponse {
	SuccessResponse { status, media_type: JSON, schema: Some(schema), description }
}

// The annotation table below writes `RequestBody` / `SuccessResponse` as
// struct literals, not through `const fn` helpers: a `&OperationAnnotation`
// built from a `const fn` call does not constant-promote, so `annotation_for`
// would fail to compile (E0515 — the reference would borrow a temporary).
// The one helper kept, `json_success_status`, is used only inside the
// `INGEST_SUCCESS` const item, where `const fn` calls are allowed.

/// One operation's wire contract, as FRE-9b annotates it.
struct OperationAnnotation {
	/// One or two sentences from the handler's own doc comment.
	description: &'static str,
	/// The handler's query struct, as [`QueryParam`]s.
	query: &'static [QueryParam],
	/// Whether the handler reads the `tag.*` selector family as well
	/// (`SelectorQuery` reads, CSV/Parquet ingest).
	selector: bool,
	/// The request body, when the handler takes one. `None` is an explicit
	/// statement — the handler takes no body (a query-only or empty POST) —
	/// not an omission; the POST-body test pins which operations may say so.
	body: Option<RequestBody>,
	/// The success responses (one entry, except ingest: `201` for a fresh seal,
	/// `200` for an idempotent replay of the same body).
	success: &'static [SuccessResponse],
}

/// Derive a human-readable summary from a `snake_case` `operation_id`:
/// split on `_`, capitalise the first word, and join with spaces.
fn operation_summary(id: &str) -> String {
	let mut words = id.split('_').filter(|word| !word.is_empty());
	let Some(first) = words.next() else {
		return id.to_string();
	};
	let mut buf = String::new();
	let mut chars = first.chars();
	if let Some(head) = chars.next() {
		buf.extend(head.to_uppercase());
	}
	buf.extend(chars);
	for word in words {
		buf.push(' ');
		buf.push_str(word);
	}
	buf
}

/// Derive a tag from the operation id, grouping operations by API area.
fn operation_tag(id: &str) -> &'static str {
	if id.starts_with("get_health") || id.starts_with("get_ready") {
		"Probes"
	} else if id == "get_version" {
		"Version"
	} else if id.starts_with("get_metrics") || id.starts_with("get_debug") {
		"Metrics"
	} else if id.starts_with("storage_") {
		if id.contains("ingest") {
			"Ingest"
		} else if id.starts_with("storage_aspect_declare") {
			"Declare"
		} else if id.contains("reconcile") || id.contains("quarantine") || id.contains("squash") || id.contains("compact") || id.contains("fsck") {
			"Maintenance"
		} else if id.contains("backup") || id.contains("restore") {
			"Backup"
		} else {
			"Storage Reads"
		}
	} else if id.starts_with("interpolate") || id.starts_with("downsample") {
		"Compute"
	} else if id.starts_with("grafana") {
		"Grafana"
	} else if id.starts_with("list_bench") || id.starts_with("get_bench") {
		"Benchmarks"
	} else {
		"Other"
	}
}

/// Ingest's two success responses: `201` for a fresh seal, `200` with the
/// recorded receipt when the idempotency key replays a committed batch.
const INGEST_SUCCESS: &[SuccessResponse] = &[json_success_status(201, "IngestResponse", "The sealed segment's descriptor summary: the segment id, frame version, row counts, on-disk size, timestamp span and the series the batch wrote."), json_success_status(200, "IngestResponse", "An idempotent replay: the request's key was already committed with the same batch, so nothing was written again and the recorded receipt is answered back (with the `Idempotency-Replayed: true` header).")];

/// The annotation for one operation, by its frozen `operation_id`. Exhaustive:
/// an operation with no arm here is unannotated, and the annotation-coverage
/// test fails the build on it.
fn annotation_for(operation_id: &str) -> Option<&'static OperationAnnotation> {
	let annotation = match operation_id {
		// Probes (anonymous: the only scope an unauthenticated orchestrator needs).
		"get_health" => &OperationAnnotation { description: "Liveness probe: the process is up and can serve a request.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("HealthResponse"), description: "Always `\"ok\"` while the process can serve a request, with the service name and build version." }] },
		"get_health_z" => &OperationAnnotation { description: "Liveness probe, under the `/healthz` alias so k8s probes work the same way on both listeners.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("HealthResponse"), description: "Always `\"ok\"` while the process can serve a request, with the service name and build version." }] },
		"get_ready" => &OperationAnnotation { description: "Readiness probe: the service is ready to accept traffic. Reports whether a segment store is configured, whether the store is write-poisoned, and the store's identity; answers `503` when the store reports `incompatible_store`.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("ReadyResponse"), description: "Whether the service can accept traffic, with the store's identity, durability class and recovery counts when a store is configured." }] },
		"get_ready_z" => &OperationAnnotation { description: "Readiness probe, under the `/readyz` alias so k8s probes work the same way on both listeners.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("ReadyResponse"), description: "Whether the service can accept traffic, with the store's identity, durability class and recovery counts when a store is configured." }] },
		"get_version" => &OperationAnnotation { description: "Which build this is, which store it serves, and what its exit statuses mean. Needs the read scope: it names layout numbers, the store's UUID and the scope it serves, which an unauthenticated probe must not hand out.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("VersionResponse"), description: "The service name and build version, how the binary was built, the store it serves, the scope pin, and the C7 exit-status table." }] },
		// Operational telemetry.
		"get_metrics" => &OperationAnnotation { description: "The Prometheus surface: the server's counters and latency histograms as exposition text, plus the store's disk headroom.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: PROMETHEUS, schema: None, description: "The counters and latency histograms as Prometheus exposition text." }] },
		"get_debug_profile_current" => &OperationAnnotation { description: "The live counter and latency-quantile snapshot as JSON, so a benchmark run can read the p95 target directly.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("ProfileReport"), description: "The counter snapshot plus the estimated p50/p95/p99 latency for each instrumented compute endpoint." }] },
		"storage_headroom" => &OperationAnnotation { description: "The store's free-space margin: the filesystem holding the store and the margin the disk guard leaves on it.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("HeadroomResponse"), description: "The filesystem's size, the bytes an unprivileged writer can still use, the reserve, what a write may still take, and whether the guard refuses writes now." }] },
		// Benchmark runs.
		"list_bench_runs" => &OperationAnnotation { description: "Every benchmark run manifest the run directory holds, newest first, addressed by content hash.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("RunListResponse"), description: "Every run found, newest `generated_at` first." }] },
		"get_bench_run" => &OperationAnnotation { description: "One run manifest, by its content-hash id: the 64 lower-case hex digits of the manifest's SHA-256, as `GET /bench/runs` lists them. Content-addressed: these bytes are this id forever.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("RunManifest"), description: "The run manifest JSON as the benchmark run wrote it." }] },
		// Grafana's JSON-datasource protocol.
		"grafana_test" => &OperationAnnotation { description: "The datasource's connection test.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("GrafanaTest"), description: "The connection test's answer: `{\"status\": \"ok\", \"datasource\": \"weftdb\"}`." }] },
		"grafana_metrics" => &OperationAnnotation { description: "Every declared aspect, filtered by the picker's typed prefix when it sends one, as the metric picker's options.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("GrafanaFind"), inline_type: None, description: "The picker's typed prefix, if it sent one (`{\"metric\": \"...\"}`); an empty object lists every aspect." }), success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("MetricOptions"), description: "One picker entry per matching aspect, each carrying the reduction options." }] },
		"grafana_query" => &OperationAnnotation { description: "Each visible target's stored downsample, as the series the panel draws.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("GrafanaQuery"), inline_type: None, description: "The panel's time range, interval, point budget and targets; Grafana sends more fields, which the server ignores." }), success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("GrafanaSeriesList"), description: "One answered series per visible target, as `[value, epoch_ms]` pairs, oldest first." }] },
		// Stateless compute: interpolation.
		"interpolate" => &OperationAnnotation { description: "Reconstruct an irregular series onto a regular output grid using the interpolation engine.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("InterpolateRequest"), inline_type: None, description: "The spline method, output-grid resolution, optional range, and the input samples (must be non-empty)." }), success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("InterpolateResponse"), description: "The spline and resolution actually used, the input/output point counts, the backend that produced the values, and the interpolated series on the output grid." }] },
		"interpolate_csv" => &OperationAnnotation { description: "Reconstruct an irregular series onto a regular output grid, answered as CSV.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("InterpolateRequest"), inline_type: None, description: "The spline method, output-grid resolution, optional range, and the input samples (must be non-empty)." }), success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The interpolated series as a `timestamp,value,kind` CSV document." }] },
		"interpolate_arrow" => &OperationAnnotation { description: "Reconstruct an irregular series onto a regular output grid, answered as an Arrow IPC stream.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("InterpolateRequest"), inline_type: None, description: "The spline method, output-grid resolution, optional range, and the input samples (must be non-empty)." }), success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The interpolated series as an Arrow IPC stream of one record batch." }] },
		"interpolate_parquet" => &OperationAnnotation { description: "Reconstruct an irregular series onto a regular output grid, answered as a Parquet file.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("InterpolateRequest"), inline_type: None, description: "The spline method, output-grid resolution, optional range, and the input samples (must be non-empty)." }), success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The interpolated series as a Parquet file." }] },
		"interpolate_ilp" => &OperationAnnotation { description: "Interpolate an InfluxDB Line Protocol payload onto a regular grid: parse the body as ILP, project the chosen numeric field into a series, and interpolate it.", query: ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series. The series range is the data's own timestamp span." }), success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("InterpolateResponse"), description: "The spline and resolution actually used, the input/output point counts, the backend that produced the values, and the interpolated series on the output grid." }] },
		"interpolate_ilp_csv" => &OperationAnnotation { description: "Interpolate an InfluxDB Line Protocol payload onto a regular grid, answered as CSV.", query: ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series." }), success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The interpolated series as a `timestamp,value,kind` CSV document." }] },
		"interpolate_ilp_arrow" => &OperationAnnotation { description: "Interpolate an InfluxDB Line Protocol payload onto a regular grid, answered as an Arrow IPC stream.", query: ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series." }), success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The interpolated series as an Arrow IPC stream of one record batch." }] },
		"interpolate_ilp_parquet" => &OperationAnnotation { description: "Interpolate an InfluxDB Line Protocol payload onto a regular grid, answered as a Parquet file.", query: ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series." }), success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The interpolated series as a Parquet file." }] },
		"interpolate_point" => &OperationAnnotation { description: "Evaluate the reconstructed signal at a single instant, labelling the result interpolated vs extrapolated.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("PointRequest"), inline_type: None, description: "The spline method, the single instant to evaluate at, and the input samples (must be non-empty)." }), success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("PointResponse"), description: "The spline actually used, the instant echoed back, the reconstructed value at the instant, its provenance, and the backend that produced it." }] },
		// Stateless compute: downsampling.
		"downsample" => &OperationAnnotation { description: "Reduce a series into grid-aligned buckets.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("DownsampleRequest"), inline_type: None, description: "The bucket width, the reductions to report per bucket, the optional range, the input samples (must be non-empty), and the optional width override and gap fill." }), success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("DownsampleResponse"), description: "The bucket width actually used, the reductions reported, the input/bucket counts, and the reduced series, ascending by time." }] },
		"downsample_csv" => &OperationAnnotation { description: "Reduce a series into grid-aligned buckets, answered as CSV.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("DownsampleRequest"), inline_type: None, description: "The bucket width, the reductions to report per bucket, the optional range, the input samples (must be non-empty), and the optional width override and gap fill." }), success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The reduced series as a `timestamp,count,<agg>…` CSV document." }] },
		"downsample_arrow" => &OperationAnnotation { description: "Reduce a series into grid-aligned buckets, answered as an Arrow IPC stream.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("DownsampleRequest"), inline_type: None, description: "The bucket width, the reductions to report per bucket, the optional range, the input samples (must be non-empty), and the optional width override and gap fill." }), success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The reduced series as an Arrow IPC stream of one record batch." }] },
		"downsample_parquet" => &OperationAnnotation { description: "Reduce a series into grid-aligned buckets, answered as a Parquet file.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("DownsampleRequest"), inline_type: None, description: "The bucket width, the reductions to report per bucket, the optional range, the input samples (must be non-empty), and the optional width override and gap fill." }), success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The reduced series as a Parquet file." }] },
		"downsample_ilp" => &OperationAnnotation { description: "Downsample an InfluxDB Line Protocol payload into grid-aligned buckets: parse the body as ILP, project the chosen numeric field into a series, and reduce it.", query: DOWNSAMPLE_ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series. The window is the data's own timestamp span." }), success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("DownsampleResponse"), description: "The bucket width actually used, the reductions reported, the input/bucket counts, and the reduced series, ascending by time." }] },
		"downsample_ilp_csv" => &OperationAnnotation { description: "Downsample an InfluxDB Line Protocol payload into grid-aligned buckets, answered as CSV.", query: DOWNSAMPLE_ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series." }), success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The reduced series as a `timestamp,count,<agg>…` CSV document." }] },
		"downsample_ilp_arrow" => &OperationAnnotation { description: "Downsample an InfluxDB Line Protocol payload into grid-aligned buckets, answered as an Arrow IPC stream.", query: DOWNSAMPLE_ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series." }), success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The reduced series as an Arrow IPC stream of one record batch." }] },
		"downsample_ilp_parquet" => &OperationAnnotation { description: "Downsample an InfluxDB Line Protocol payload into grid-aligned buckets, answered as a Parquet file.", query: DOWNSAMPLE_ILP_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records whose `field` values form the series." }), success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The reduced series as a Parquet file." }] },
		// Stored-data reads.
		"storage_aspects_list" => &OperationAnnotation { description: "List the store's declared aspects and the physical encoding / timestamp unit each was declared under.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("AspectListResponse"), description: "The declared aspects, in store order." }] },
		"storage_catalog" => &OperationAnnotation { description: "Report the database/subject scope the configured store is bound to, plus the full registered DB/subject hierarchy.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("CatalogResponse"), description: "The database and subject namespace this server's store is scoped to, and every database registered in the store's catalog with its subjects." }] },
		"storage_stats" => &OperationAnnotation { description: "The store-wide aggregate over every aspect's rollup — the subject-wide north-star bytes/point.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("StoreStatsResponse"), description: "The store-wide aspect/segment/row counts, on-disk bytes, bytes/point, order-health signals and timestamp span." }] },
		"storage_time_range" => &OperationAnnotation { description: "Read the rows of an aspect in the inclusive `[start, end]` window as an Arrow IPC stream (lossless decimal-text value column).", query: TIME_RANGE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The rows as an Arrow IPC stream of one record batch, typed by the aspect's declared schema." }] },
		"storage_time_range_parquet" => &OperationAnnotation { description: "Read the rows of an aspect in the inclusive `[start, end]` window as a Parquet file.", query: TIME_RANGE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The rows as a Parquet file." }] },
		"storage_time_range_csv" => &OperationAnnotation { description: "Read the rows of an aspect in the inclusive `[start, end]` window as a two-column `timestamp,value` CSV document.", query: TIME_RANGE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The rows as a two-column `timestamp,value` CSV document; a null value is an empty field." }] },
		"storage_points_list" => &OperationAnnotation { description: "The JSON counterpart of the Arrow range read: the present rows of an aspect in the window as a lossless decimal-text point list, paged.", query: POINTS_RANGE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("PointsPage"), description: "One page of the stored read: the aspect, the series' tags, the timestamp unit, the total before paging, and the rows in this page with the cursor for the next." }] },
		"storage_downsample_get" | "storage_downsample_post" => &OperationAnnotation { description: "Reduce an aspect's stored segments over `[start, end]` into grid-aligned buckets, in place — the bounded-memory cross-segment reduction. Registered on both verbs: it is a read that takes no body either way.", query: STORAGE_DOWNSAMPLE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("DownsampleResponse"), description: "The bucket width actually used, the reductions reported, the input/bucket counts, the reduced series, and the tags of the stored series reduced." }] },
		"storage_downsample_csv_get" | "storage_downsample_csv_post" => &OperationAnnotation { description: "Reduce an aspect's stored segments over `[start, end]` into grid-aligned buckets, answered as CSV. Registered on both verbs: it is a read that takes no body either way.", query: STORAGE_DOWNSAMPLE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The reduced series as a `timestamp,count,<agg>…` CSV document." }] },
		"storage_downsample_arrow_get" | "storage_downsample_arrow_post" => &OperationAnnotation { description: "Reduce an aspect's stored segments over `[start, end]` into grid-aligned buckets, answered as an Arrow IPC stream. Registered on both verbs: it is a read that takes no body either way.", query: STORAGE_DOWNSAMPLE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The reduced series as an Arrow IPC stream of one record batch." }] },
		"storage_downsample_parquet_get" | "storage_downsample_parquet_post" => &OperationAnnotation { description: "Reduce an aspect's stored segments over `[start, end]` into grid-aligned buckets, answered as a Parquet file. Registered on both verbs: it is a read that takes no body either way.", query: STORAGE_DOWNSAMPLE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The reduced series as a Parquet file." }] },
		"storage_interpolate_get" => &OperationAnnotation { description: "Interpolate an aspect's stored segments onto a regular grid, reading the window's knots with a margin outside it.", query: STORED_INTERPOLATE_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("StoredInterpolateResponse"), description: "The aspect, the time unit, the grid's bounds and margin, the spline and resolution actually used, and the reconstructed series with provenance." }] },
		"storage_interpolate_csv_get" => &OperationAnnotation { description: "Interpolate an aspect's stored segments onto a regular grid, answered as CSV.", query: STORED_INTERPOLATE_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The reconstructed series as a `timestamp,value,kind` CSV document." }] },
		"storage_interpolate_arrow_get" => &OperationAnnotation { description: "Interpolate an aspect's stored segments onto a regular grid, answered as an Arrow IPC stream.", query: STORED_INTERPOLATE_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The reconstructed series as an Arrow IPC stream of one record batch." }] },
		"storage_interpolate_parquet_get" => &OperationAnnotation { description: "Interpolate an aspect's stored segments onto a regular grid, answered as a Parquet file.", query: STORED_INTERPOLATE_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The reconstructed series as a Parquet file." }] },
		"storage_point_at" => &OperationAnnotation { description: "The present value of an aspect at exactly timestamp `t`, or a `found: false` miss.", query: POINT_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("StoredPointResponse"), description: "The aspect, the series' tags, the timestamp unit, the instant echoed back, the present value as lossless decimal text, and whether one was found." }] },
		"storage_points_at_multi" => &OperationAnnotation { description: "The batch counterpart of the point lookup: resolve many instants in one pass, with one result per queried instant in query order.", query: MULTI_POINT_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("StoredPointsResponse"), description: "The aspect, the series' tags, the timestamp unit, and one result per queried instant, aligned to the `t` list." }] },
		"storage_value_range" => &OperationAnnotation { description: "Read the present rows of an aspect whose value falls in the inclusive `[lo, hi]` band as an Arrow IPC stream (lossless decimal-text value column).", query: VALUE_RANGE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: ARROW, schema: None, description: "The rows as an Arrow IPC stream of one record batch, typed by the aspect's declared schema." }] },
		"storage_value_range_parquet" => &OperationAnnotation { description: "Read the present rows of an aspect whose value falls in the inclusive `[lo, hi]` band as a Parquet file.", query: VALUE_RANGE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: PARQUET, schema: None, description: "The rows as a Parquet file." }] },
		"storage_value_range_csv" => &OperationAnnotation { description: "Read the present rows of an aspect whose value falls in the inclusive `[lo, hi]` band as a two-column `timestamp,value` CSV document.", query: VALUE_RANGE_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: CSV, schema: None, description: "The rows as a two-column `timestamp,value` CSV document; a null value is an empty field." }] },
		"storage_value_points" => &OperationAnnotation { description: "The JSON counterpart of the Arrow value-range read: the present rows in the band as a lossless decimal-text point list, paged.", query: VALUE_POINTS_QUERY, selector: true, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("PointsPage"), description: "One page of the stored read: the aspect, the series' tags, the timestamp unit, the total before paging, and the rows in this page with the cursor for the next." }] },
		"storage_aspect_stats" => &OperationAnnotation { description: "One aspect's materialized segment-set rollup, including the north-star bytes/point. An aspect with no sealed segments reports the empty rollup, not an error.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("AspectStatsResponse"), description: "The aspect's segment/row counts, on-disk bytes, bytes/point, order-health signals and timestamp/value spans." }] },
		"storage_aspect_schema" => &OperationAnnotation { description: "One aspect's declared schema, the single-aspect read counterpart of the aspect list and the declaration.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("AspectSchemaResponse"), description: "The declared aspect and its physical encoding, tolerance, timestamp unit and upsert keys." }] },
		// Point writes, one route per wire format.
		"storage_ingest_points" => &OperationAnnotation { description: "Seal a batch of points into the aspect's declared schema. The `Idempotency-Key` header (or `batch_id` in the body) makes the seal idempotent: a replayed key answers `200` with the recorded receipt instead of sealing again.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("IngestRequest"), inline_type: None, description: "The points to seal in caller order, the optional page height, the sortedness requirement, the idempotency key, and the tags naming the one series the batch writes." }), success: INGEST_SUCCESS },
		"storage_ingest_ilp" => &OperationAnnotation { description: "Seal an InfluxDB Line Protocol payload into the aspect's declared schema. The payload is the request body (`text/plain`); the field, measurement, precision and page options are query parameters.", query: ILP_INGEST_QUERY, selector: false, body: Some(RequestBody { media_type: TEXT, schema: None, inline_type: Some("string"), description: "The ILP payload: line-protocol records of the `measurement` carrying `field`, sharing one tag set, which names the series the batch writes." }), success: INGEST_SUCCESS },
		"storage_ingest_csv" => &OperationAnnotation { description: "Seal a CSV payload into the aspect's declared schema. Rows are `timestamp,value` lines; `tag.<key>=value` query parameters name the one series the batch writes.", query: CSV_INGEST_QUERY, selector: true, body: Some(RequestBody { media_type: CSV, schema: None, inline_type: Some("string"), description: "The CSV payload: `timestamp,value` rows, one per line, with the `timestamp,value` header." }), success: INGEST_SUCCESS },
		"storage_ingest_parquet" => &OperationAnnotation { description: "Seal a Parquet batch into the aspect's declared schema. `tag.<key>=value` query parameters name the one series the batch writes.", query: PARQUET_INGEST_QUERY, selector: true, body: Some(RequestBody { media_type: PARQUET, schema: None, inline_type: Some("string"), description: "The Parquet file: a `timestamp` column and a value column, one batch per request." }), success: INGEST_SUCCESS },
		// Catalog writes.
		"storage_aspect_declare" => &OperationAnnotation { description: "Declare an aspect's schema: its name, physical encoding, timestamp unit, tolerance and upsert keys. A declaration is immutable once made; a re-declaration with a different schema is `409 schema_conflict`. Answers `201 Created` with the declared schema read back.", query: NO_QUERY, selector: false, body: Some(RequestBody { media_type: JSON, schema: Some("DeclareAspectRequest"), inline_type: None, description: "The aspect name, the physical encoding token, the fractional-decimal-digit scale (required for the fixed-point encodings), the permitted per-value error as decimal text, the timestamp resolution token, and the upsert keys." }), success: &[SuccessResponse { status: 201, media_type: JSON, schema: Some("DeclareAspectResponse"), description: "The declared aspect: its name, physical encoding, scale, tolerance, timestamp unit and upsert keys." }] },
		// Store maintenance.
		"storage_reconcile_store" => &OperationAnnotation { description: "Sweep every declared aspect's out-of-order segments into time-sorted ones. A per-aspect failure is listed in the body rather than failing the whole sweep.", query: RECONCILE_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("ReconcileStoreResponse"), description: "The threshold and mode applied, how many aspects were scanned and reconciled, how many segments were rewritten, the post-sweep order-health counts, and the aspects whose pass failed or whose breaker is tripped." }] },
		"storage_reconcile_aspect" => &OperationAnnotation { description: "Rewrite every out-of-order segment of one aspect into a time-sorted one, so afterward point lookups over the aspect binary-search.", query: RECONCILE_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("ReconcileResponse"), description: "The aspect, the sweep mode applied, whether the pass triggered, how many segments it rewrote, and the post-pass order-health counts." }] },
		"storage_squash_aspect" => &OperationAnnotation { description: "Fold an aspect's segments into one.", query: SQUASH_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("SquashResponse"), description: "The aspect, whether the squash triggered, how many segments it removed, and the post-pass segment count." }] },
		"storage_compact_aspect" => &OperationAnnotation { description: "Coalesce an aspect's segments toward ~`target_rows` rows per segment, leaving already-large segments untouched.", query: COMPACT_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("CompactResponse"), description: "The aspect, how many segments coalescing removed, and the post-pass segment count." }] },
		"storage_fsck" => &OperationAnnotation { description: "Run the recovery pass over the store and report what it found and did — counts only, never segment contents.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("RecoveryReport"), description: "What one recovery pass found and did: rows checked, quarantined, suspected, re-bound and adopted, orphans, and whether any suspect record's cause is a frame version this build predates." }] },
		"storage_quarantine_list" => &OperationAnnotation { description: "List the quarantine records the recovery pass left, optionally restricted to one aspect.", query: QUARANTINE_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("QuarantineRecords"), description: "The quarantine records, each naming the aspect, segment id and generation, the quarantined file, its class and reason, its time span and series, and the row to put back on release." }] },
		"storage_quarantine_purge_expired" => &OperationAnnotation { description: "Purge the quarantine records past their TTL.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("PurgeExpiredResponse"), description: "How many expired quarantine records were purged." }] },
		"storage_quarantine_release" => &OperationAnnotation { description: "Release a quarantined segment back into the index, exactly as it was.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("QuarantineRecord"), description: "The released record: the aspect, segment id and generation, the quarantined file, its class and reason, its time span and series." }] },
		"storage_quarantine_purge" => &OperationAnnotation { description: "Purge one quarantined segment.", query: NO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("QuarantineRecord"), description: "The purged record: the aspect, segment id and generation, the quarantined file, its class and reason, its time span and series." }] },
		// Backup and the restore drill.
		"storage_backup" => &OperationAnnotation { description: "Snapshot the whole store into a labelled subdirectory under the backup root, and verify the copy.", query: BACKUP_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("BackupResponse"), description: "The snapshot directory, the per-database verification of the copy, the total rows and bytes, the verification mode, and how long the snapshot took." }] },
		"storage_restore_drill" => &OperationAnnotation { description: "Rehearse restoring a backup into a throwaway directory, verify every restored database at its destination, and report what came back. The live store is never touched and nothing is overwritten.", query: RESTORE_DRILL_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("RestoreDrillResponse"), description: "The backup rehearsed, the per-database verification of each restored file, the total rows and bytes, how long the rehearsal took, and whether the copy could be removed afterwards." }] },
		// Backup and the restore into a new root.
		"storage_restore_into" => &OperationAnnotation { description: "Restore a backup into a caller-chosen new root under the restore base directory, verify every restored database at its destination, and report what came back. The live store is never touched. Adoption is deferred to the restored root's first server open, or the offline CLI's `--adopt`.", query: RESTORE_INTO_QUERY, selector: false, body: None, success: &[SuccessResponse { status: 200, media_type: JSON, schema: Some("RestoreIntoResponse"), description: "The backup restored, the mode (whole_store or control_plane), the restored root, the per-database verification of each restored file, the total rows and bytes, and the next-step guidance." }] },
		_ => return None,
	};
	Some(annotation)
}

/// The `OpenAPI` method key (lowercase) for an HTTP method.
const fn method_key(method: &Method) -> &'static str {
	match *method {
		Method::GET => "get",
		Method::POST => "post",
		Method::PUT => "put",
		Method::PATCH => "patch",
		Method::DELETE => "delete",
		Method::HEAD => "head",
		Method::OPTIONS => "options",
		_ => "other",
	}
}

/// Build the `ErrorEnvelope` schema entry.
fn error_envelope_schema() -> Value {
	json!({
		"type": "object",
		"description": "The standard error envelope (plan §4). Every error response carries this shape; `code` names the exact error, `message` is a human-readable description, and `details` carries structured context when present.",
		"properties": {
			"code": { "type": "string", "description": "The §4 error code, e.g. `bad_request`, `route_not_found`, `internal`" },
			"message": { "type": "string", "description": "A human-readable description of what went wrong" },
			"details": { "type": "object", "description": "Structured context specific to the error (absent when empty)", "nullable": true },
			"request_id": { "type": "string", "description": "The opaque request id, present in `5xx` responses" },
			"limit": { "type": "integer", "description": "The server-side cap that was exceeded (present for rate-limited or size-limited errors)", "nullable": true },
			"committed": { "type": "boolean", "description": "Whether the write was committed despite the error (present for outcome-unknown commits)", "nullable": true },
			"retry_after": { "type": "integer", "description": "The suggested seconds before retrying (present for overload/busy errors)", "nullable": true },
		},
	})
}

/// Build the `responses` entry for one error status.
fn error_response_entry(status: u16) -> Value {
	let desc = error_status_description(status);
	json!({
		"description": desc,
		"content": {
			"application/json": {
				"schema": { "$ref": "#/components/schemas/ErrorEnvelope" },
			},
		},
	})
}

/// Build one operation object for a route entry's spec, from its FRE-9b
/// annotation: the description, the query parameters, the request body, the
/// success responses, and the `security` the scope demands (SEC-6: the
/// anonymous principal may call only the probe scope; every other scope
/// needs `Authorization: Bearer <key>` when authentication is on).
fn build_operation(spec: &RouteSpec) -> Value {
	let annotation = annotation_for(spec.operation_id).expect("every registered operation carries an FRE-9b annotation; add one to annotation_for");
	let summary = operation_summary(spec.operation_id);
	let tag = operation_tag(spec.operation_id);

	let mut responses: Map<String, Value> = Map::new();
	for success in annotation.success {
		let mut entry = json!({"description": success.description});
		let content_schema = match success.schema {
			Some(name) => json!({ "$ref": format!("#/components/schemas/{name}") }),
			None if success.media_type == ARROW || success.media_type == PARQUET => json!({ "type": "string", "format": "binary" }),
			None => json!({ "type": "string" }),
		};
		entry.as_object_mut().expect("response entry is an object").insert("content".to_string(), json!({ success.media_type: { "schema": content_schema } }));
		responses.insert(success.status.to_string(), entry);
	}
	for &status in ERROR_STATUSES {
		responses.insert(status.to_string(), error_response_entry(status));
	}

	let mut result = Map::new();
	result.insert("operationId".to_string(), Value::String(spec.operation_id.to_string()));
	result.insert("summary".to_string(), Value::String(summary));
	result.insert("description".to_string(), Value::String(annotation.description.to_string()));
	result.insert("tags".to_string(), Value::Array(vec![Value::String(tag.to_string())]));
	let mut parameters: Vec<Value> = Vec::new();
	for param in annotation.query {
		parameters.push(json!({
			"name": param.name,
			"in": "query",
			"required": param.required,
			"description": param.description,
			"schema": { "type": param.ty },
		}));
	}
	if annotation.selector {
		for param in SELECTOR_PARAMS {
			parameters.push(json!({
				"name": param.name,
				"in": "query",
				"required": param.required,
				"description": param.description,
				"schema": { "type": param.ty },
			}));
		}
	}
	if !parameters.is_empty() {
		result.insert("parameters".to_string(), Value::Array(parameters));
	}
	if let Some(body) = &annotation.body {
		let schema = match body.schema {
			Some(name) => json!({ "$ref": format!("#/components/schemas/{name}") }),
			None if body.media_type == PARQUET => json!({ "type": "string", "format": "binary" }),
			None => json!({ "type": body.inline_type.unwrap_or("string") }),
		};
		result.insert(
			"requestBody".to_string(),
			json!({
				"description": body.description,
				"required": true,
				"content": { body.media_type: { "schema": schema } },
			}),
		);
	}
	result.insert("responses".to_string(), Value::Object(responses));
	if spec.scope == Scope::Probe {
		result.insert("security".to_string(), json!([]));
	} else {
		result.insert("security".to_string(), json!([{ "BearerAuth": [] }]));
	}
	result.insert("x-weft-scope".to_string(), Value::String(spec.scope.as_str().to_string()));
	result.insert("x-weft-audit".to_string(), Value::String(spec.audit.as_str().to_string()));
	result.insert("x-weft-operator-detail".to_string(), json!(spec.operator_detail));
	result.insert("x-weft-tier".to_string(), Value::String(spec.tier.label().to_string()));
	result.insert("x-weft-admission-class".to_string(), Value::String(spec.admission_class.as_str().to_string()));
	Value::Object(result)
}

/// Extract path parameters from a path template: segments wrapped in
/// `{` `}` become `OpenAPI` path-parameter entries.
fn extract_path_parameters(template: &str) -> Vec<Value> {
	let mut params: Vec<Value> = Vec::new();
	for segment in template.split('/') {
		if segment.starts_with('{') && segment.ends_with('}') {
			let name: &str = &segment[1..segment.len() - 1];
			params.push(json!({
				"name": name,
				"in": "path",
				"required": true,
				"schema": { "type": "string" },
			}));
		}
	}
	params
}

/// The `PointsPage` schema — the JSON response body for stored-range reads
/// (`StoredRangeResponse` in `storage.rs`).
fn points_page_schema() -> Value {
	json!({
		"type": "object",
		"description": "One page of a stored read.",
		"properties": {
			"aspect": { "type": "string" },
			"tags": { "type": ["object", "null"], "description": "The tags of the series read (TAG-10)", "additionalProperties": { "type": "string" } },
			"time_unit": { "type": "string", "description": "The aspect's declared timestamp unit token" },
			"total": { "type": ["integer", "null"], "description": "Rows in the whole window, before paging. Present on the first page; omitted or null on cursor-following pages." },
			"count": { "type": "integer" },
			"offset": { "type": "integer" },
			"limit": { "type": ["integer", "null"] },
			"page": { "type": ["integer", "null"] },
			"next_cursor": { "type": ["string", "null"], "description": "Opaque forward-iteration cursor for the next page; absent at the end." },
			"points": { "type": "array", "items": { "$ref": "#/components/schemas/StoredPoint" } }
		},
		"required": ["aspect", "time_unit", "count", "offset", "points"]
	})
}

/// One stored row in the JSON range response: an integer timestamp and its
/// value as lossless decimal text, or `null` for a null row.
fn stored_point_schema() -> Value {
	json!({
		"type": "object",
		"description": "One stored row in the JSON range response.",
		"properties": {
			"timestamp": { "type": "integer", "description": "Row timestamp (epoch integer in the aspect's declared unit)." },
			"value": { "type": ["string", "null"], "description": "The stored value as decimal text, or `null` for a null row. Decimal text keeps every digit (no float round-trip)." }
		},
		"required": ["timestamp"]
	})
}

/// Build the `components` section: one schema per request/response body the
/// annotations name, the shared error responses, and the `BearerAuth` scheme
/// (SEC-6: an API key minted by `weft-server keygen`, sent as
/// `Authorization: Bearer <key>`).
/// Stateless compute: the spline/resolution selectors, the input point, and the interpolate request/response shapes.
fn add_compute_request_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"SplineSpec".to_string(),
		json!({
			"description": "Spline method selector, with stable lowercase JSON tokens (`polynomial` carries its parameters).",
			"oneOf": [
				{ "type": "string", "enum": ["linear", "quadratic", "cubic"] },
				{ "type": "object", "properties": { "polynomial": { "type": "object", "properties": { "degree": { "type": "integer" }, "bounds_factor": { "type": ["number", "null"] } }, "required": ["degree"] } }, "required": ["polynomial"] }
			]
		}),
	);
	schemas.insert(
		"ResolutionSpec".to_string(),
		json!({
			"type": "string",
			"description": "Output-grid resolution selector, with stable lowercase JSON tokens.",
			"enum": ["nanoseconds", "microseconds", "milliseconds", "seconds", "minutes", "hours", "days", "weeks", "months", "years"]
		}),
	);
	schemas.insert(
		"InputPoint".to_string(),
		json!({
			"type": "object",
			"description": "One input sample.",
			"properties": {
				"timestamp": { "type": "string", "format": "date-time", "description": "Sample timestamp (RFC 3339)." },
				"value": { "type": "number", "description": "Sample value, widened to `BigDecimal` before the engine sees it." }
			},
			"required": ["timestamp", "value"]
		}),
	);
	schemas.insert(
		"InterpolateRequest".to_string(),
		json!({
			"type": "object",
			"description": "Request body for `POST /api/v1/interpolate`.",
			"properties": {
				"spline": { "$ref": "#/components/schemas/SplineSpec", "description": "Spline method (default cubic)." },
				"resolution": { "$ref": "#/components/schemas/ResolutionSpec", "description": "Output-grid resolution (default minutes)." },
				"start": { "type": ["string", "null"], "format": "date-time", "description": "Optional inclusive range start; defaults to the earliest input timestamp." },
				"end": { "type": ["string", "null"], "format": "date-time", "description": "Optional inclusive range end; defaults to the latest input timestamp." },
				"points": { "type": "array", "items": { "$ref": "#/components/schemas/InputPoint" }, "description": "Input samples (must be non-empty)." }
			},
			"required": ["points"]
		}),
	);
	schemas.insert(
		"OutputPoint".to_string(),
		json!({
			"type": "object",
			"description": "One interpolated value on the output grid.",
			"properties": {
				"timestamp": { "type": "string", "format": "date-time", "description": "Output-grid timestamp." },
				"value": { "type": "number", "description": "Interpolated value (a value that is not finite fails the request with `not_finite` — no silent substitute)." },
				"kind": { "$ref": "#/components/schemas/ValueKind", "description": "Provenance of this value." }
			},
			"required": ["timestamp", "value", "kind"]
		}),
	);
	schemas.insert(
		"ValueKind".to_string(),
		json!({
			"type": "string",
			"description": "A value's provenance: `raw` (coincides with an input), `interpolated` (within the observed span), or `extrapolated` (outside it).",
			"enum": ["raw", "interpolated", "extrapolated"]
		}),
	);
}

/// Stateless compute: the downsample request/response shapes.
fn add_compute_downsample_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"InterpolateResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/interpolate`.",
			"properties": {
				"spline": { "type": "string", "description": "Spline method actually used: the requested one, or the simpler one the engine stepped down to because there were too few distinct input timestamps." },
				"resolution": { "type": "string", "description": "Output-grid resolution actually used." },
				"input_points": { "type": "integer", "description": "Number of input samples accepted." },
				"output_points": { "type": "integer", "description": "Number of interpolated samples produced." },
				"backend": { "type": "string", "description": "The compute backend that actually produced the values: `cpu`, `parallel` or `gpu` (never the configured `auto` — the one that ran)." },
				"gpu_fallback": { "type": "boolean", "description": "Whether the configured `auto` backend tried the GPU for this request and fell back to the CPU pool when it failed." },
				"points": { "type": "array", "items": { "$ref": "#/components/schemas/OutputPoint" }, "description": "The interpolated series on the output grid." }
			},
			"required": ["spline", "resolution", "input_points", "output_points", "backend", "gpu_fallback", "points"]
		}),
	);
	schemas.insert(
		"PointRequest".to_string(),
		json!({
			"type": "object",
			"description": "Request body for `POST /api/v1/interpolate/point`.",
			"properties": {
				"spline": { "$ref": "#/components/schemas/SplineSpec", "description": "Spline method (default cubic)." },
				"instant": { "type": "string", "format": "date-time", "description": "The single instant to evaluate the reconstructed signal at." },
				"points": { "type": "array", "items": { "$ref": "#/components/schemas/InputPoint" }, "description": "Input samples (must be non-empty)." }
			},
			"required": ["instant", "points"]
		}),
	);
	schemas.insert(
		"PointResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/interpolate/point`.",
			"properties": {
				"spline": { "type": "string", "description": "Spline method actually used." },
				"instant": { "type": "string", "format": "date-time", "description": "The instant evaluated, echoed back." },
				"input_points": { "type": "integer", "description": "Number of input samples accepted." },
				"value": { "type": "number", "description": "The reconstructed value at the instant (narrowed from `BigDecimal`; a value with no finite `f64` representation fails the request)." },
				"kind": { "$ref": "#/components/schemas/ValueKind", "description": "The value's provenance." },
				"backend": { "type": "string", "description": "The compute backend that actually produced the value: `cpu`, `parallel` or `gpu`." },
				"gpu_fallback": { "type": "boolean", "description": "Whether the configured `auto` backend tried the GPU for this request and fell back to the CPU pool when it failed." }
			},
			"required": ["spline", "instant", "input_points", "value", "kind", "backend", "gpu_fallback"]
		}),
	);
}

/// Stateless compute: the downsample request/response shapes.
fn add_downsample_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"DownsampleRequest".to_string(),
		json!({
			"type": "object",
			"description": "Request body for `POST /api/v1/downsample`.",
			"properties": {
				"resolution": { "$ref": "#/components/schemas/ResolutionSpec", "description": "Bucket width (default minutes), aligned to the epoch grid." },
				"aggregations": { "type": "array", "items": { "type": "string" }, "description": "Reductions to report per bucket; defaults to `[min, max, avg]` when omitted or empty. Tokens: `min/max/avg/sum/first/last/p50/p90/p95/p99/twa/twa_linear/twa_bucket_end/sketch_p50/sketch_p90/sketch_p95/sketch_p99` (plus the `median`/`time_weighted_avg` aliases)." },
				"start": { "type": ["string", "null"], "format": "date-time", "description": "Optional inclusive range start; defaults to the earliest input timestamp." },
				"end": { "type": ["string", "null"], "format": "date-time", "description": "Optional inclusive range end; defaults to the latest input timestamp." },
				"points": { "type": "array", "items": { "$ref": "#/components/schemas/InputPoint" }, "description": "Input samples (must be non-empty)." },
				"width": { "type": ["string", "null"], "description": "A bucket width that is a whole number of steps (`5s`, `15m`, `6h`); overrides `resolution` when given." },
				"fill": { "type": ["string", "null"], "description": "Gap fill: emit every grid step between the bounds, the empty ones synthesized by this method (`null`, `prev`, `linear`, `quadratic`, `cubic` or a constant); omitted, only non-empty buckets are emitted." }
			},
			"required": ["points"]
		}),
	);
	schemas.insert(
		"DownsampleBucket".to_string(),
		json!({
			"type": "object",
			"description": "One grid-aligned bucket of a downsample.",
			"properties": {
				"timestamp": { "type": "string", "format": "date-time", "description": "Grid-aligned bucket start timestamp." },
				"count": { "type": "integer", "description": "Number of input samples that fell in this bucket." },
				"aggregations": { "type": "object", "description": "The requested reductions, keyed by their wire name.", "additionalProperties": { "type": "number" } }
			},
			"required": ["timestamp", "count", "aggregations"]
		}),
	);
	schemas.insert(
		"DownsampleResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/downsample`.",
			"properties": {
				"resolution": { "type": "string", "description": "Bucket width actually used: a resolution name (`minutes`), or a count and one (`15 minutes`)." },
				"aggregations": { "type": "array", "items": { "type": "string" }, "description": "Reductions reported, in request order." },
				"input_points": { "type": "integer", "description": "Number of input samples accepted (within the range)." },
				"buckets": { "type": "integer", "description": "Number of buckets in `series` (non-empty ones, plus the synthesized ones under `fill`)." },
				"series": { "type": "array", "items": { "$ref": "#/components/schemas/DownsampleBucket" }, "description": "The reduced series, ascending by time: one entry per non-empty bucket, and under `fill` one per grid step, a synthesized one with `count == 0`." },
				"fill": { "type": ["string", "null"], "description": "The fill method, when the request named one." },
				"tags": { "type": ["object", "null"], "description": "The tags of the stored series reduced (TAG-10), on the stored downsample only; absent from the compute endpoint.", "additionalProperties": { "type": "string" } }
			},
			"required": ["resolution", "aggregations", "input_points", "buckets", "series"]
		}),
	);
}

/// Stored-data reads: point pages, interpolate responses, and aspect/store descriptors.
fn add_stored_read_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"StoredPointResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/{aspect}/at` — a single-instant point lookup.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect read." },
				"tags": { "type": ["object", "null"], "description": "The tags of the series read (TAG-10): `{}` for the untagged series 0, `null` when the `tag.*` selector matched no series.", "additionalProperties": { "type": "string" } },
				"time_unit": { "type": "string", "description": "The aspect's declared timestamp unit token (e.g. `\"seconds\"`)." },
				"timestamp": { "type": "integer", "description": "The instant queried (echoed back, epoch integer in the declared unit)." },
				"value": { "type": ["string", "null"], "description": "The present value at `timestamp` as lossless decimal text, or `null` when no present row carries it." },
				"found": { "type": "boolean", "description": "Whether a present value was found at the instant — `false` distinguishes a genuine miss from a present-but-omitted `value`." }
			},
			"required": ["aspect", "time_unit", "timestamp", "found"]
		}),
	);
	schemas.insert(
		"StoredPointEntry".to_string(),
		json!({
			"type": "object",
			"description": "One instant's result in a `StoredPointsResponse`, echoed in the query order.",
			"properties": {
				"timestamp": { "type": "integer", "description": "The instant queried (epoch integer in the declared unit)." },
				"value": { "type": ["string", "null"], "description": "The present value as lossless decimal text, or omitted on a miss." },
				"found": { "type": "boolean", "description": "Whether a present value was found at the instant." }
			},
			"required": ["timestamp", "found"]
		}),
	);
	schemas.insert(
		"StoredPointsResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/{aspect}/at-multi` — a batch point lookup.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect read." },
				"tags": { "type": ["object", "null"], "description": "The tags of the series read (TAG-10).", "additionalProperties": { "type": "string" } },
				"time_unit": { "type": "string", "description": "The aspect's declared timestamp unit token." },
				"points": { "type": "array", "items": { "$ref": "#/components/schemas/StoredPointEntry" }, "description": "One result per queried instant, aligned to the `t` list." }
			},
			"required": ["aspect", "time_unit", "points"]
		}),
	);
	schemas.insert(
		"StoredInterpolateResponse".to_string(),
		json!({
			"type": "object",
			"description": "The JSON response of `GET /api/v1/storage/{aspect}/interpolate`.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect read." },
				"time_unit": { "type": "string", "description": "The aspect's declared time unit, the unit of `start`, `end` and `margin`." },
				"start": { "type": "integer", "description": "The grid's first instant, as requested (aspect unit)." },
				"end": { "type": "integer", "description": "The grid's last instant, as requested (aspect unit)." },
				"margin": { "type": "integer", "description": "The margin applied (aspect unit): the request's, or the default `end − start`." },
				"spline": { "type": "string", "description": "The spline splimes ran: the requested one, or the simpler one it stepped down to when the window held too few distinct knots." },
				"resolution": { "type": "string", "description": "The grid resolution." },
				"backend": { "type": "string", "description": "The backend splimes ran on (`Cpu`, `Parallel`, `Gpu`)." },
				"output_points": { "type": "integer", "description": "Number of grid points." },
				"points": { "type": "array", "items": { "$ref": "#/components/schemas/OutputPoint" }, "description": "The reconstructed series on the grid, with provenance." }
			},
			"required": ["aspect", "time_unit", "start", "end", "margin", "spline", "resolution", "backend", "output_points", "points"]
		}),
	);
}

/// Stored-data reads: aspect descriptors and read responses.
fn add_aspect_read_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"AspectInfo".to_string(),
		json!({
			"type": "object",
			"description": "One declared aspect's identity.",
			"properties": {
				"name": { "type": "string", "description": "The aspect name (unique within the store's `(database, subject)` scope)." },
				"physical_type": { "type": "string", "description": "The declared physical encoding token (e.g. `\"f64\"`, `\"scaled_i64\"`)." },
				"scale": { "type": ["integer", "null"], "description": "The declared fractional-decimal-digit scale, present only for the fixed-point encodings (`scaled_i64`, `scaled_i128`)." },
				"value_tolerance": { "type": "string", "description": "The declared per-value error tolerance (decimal text — `\"0\"` for an exact encoding)." },
				"timestamp_unit": { "type": "string", "description": "The declared timestamp unit token (e.g. `\"seconds\"`, `\"millis\"`)." },
				"upsert_keys": { "type": ["array", "null"], "items": { "type": "string" }, "description": "The declared upsert keys: `\"timestamp\"` first, then the tag keys every row's tags must fall inside. Absent when the aspect declares none." }
			},
			"required": ["name", "physical_type", "value_tolerance", "timestamp_unit"]
		}),
	);
	schemas.insert(
		"AspectListResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/aspects`.",
			"properties": { "aspects": { "type": "array", "items": { "$ref": "#/components/schemas/AspectInfo" }, "description": "The declared aspects, in store order." } },
			"required": ["aspects"]
		}),
	);
	schemas.insert(
		"AspectSchemaResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/{aspect}/schema`.",
			"properties": { "aspect": { "$ref": "#/components/schemas/AspectInfo", "description": "The declared aspect and its physical encoding / tolerance / timestamp unit." } },
			"required": ["aspect"]
		}),
	);
	schemas.insert(
		"AspectStatsResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/{aspect}/stats` — the materialized segment-set rollup.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect this rollup describes." },
				"segment_count": { "type": "integer", "description": "Number of sealed segments." },
				"total_rows": { "type": "integer", "description": "Total rows (present and null) across every segment." },
				"total_nulls": { "type": "integer", "description": "Total null rows across every segment." },
				"total_bytes": { "type": "integer", "description": "Total realized on-disk bytes across every `.weftseg` frame." },
				"bytes_per_point": { "type": "number", "description": "The north-star cost term: total framed bytes over total rows (0 when empty)." },
				"unsorted_segments": { "type": "integer", "description": "The number of sealed segments whose timestamps are not monotonic non-decreasing — an order-health signal." },
				"overlapping_segments": { "type": "integer", "description": "The number of sealed segments whose time window overlaps at least one other segment's — the cross-segment order-health signal." },
				"time_range": { "type": ["array", "null"], "items": { "type": "integer" }, "description": "Inclusive `[min, max]` timestamp span, or `null` when the aspect holds no non-empty segment." },
				"value_range": { "type": ["array", "null"], "items": { "type": "string" }, "description": "Inclusive `[min, max]` value span as decimal text, or `null` when the aspect holds no value-bearing segment." }
			},
			"required": ["aspect", "segment_count", "total_rows", "total_nulls", "total_bytes", "bytes_per_point", "unsorted_segments", "overlapping_segments", "time_range", "value_range"]
		}),
	);
}

/// Stored-data reads: catalog and headroom responses.
fn add_catalog_headroom_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"StoreStatsResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/stats` — the store-wide aggregate.",
			"properties": {
				"aspect_count": { "type": "integer", "description": "Number of aspects with a materialized rollup." },
				"segment_count": { "type": "integer", "description": "Total sealed segments across every aspect." },
				"total_rows": { "type": "integer", "description": "Total rows (present and null) across every aspect." },
				"total_nulls": { "type": "integer", "description": "Total null rows across every aspect." },
				"total_bytes": { "type": "integer", "description": "Total realized on-disk bytes across every aspect's `.weftseg` frames." },
				"bytes_per_point": { "type": "number", "description": "The store-wide north-star cost term: total framed bytes over total rows." },
				"unsorted_segments": { "type": "integer", "description": "Total out-of-order segments across every aspect." },
				"overlapping_segments": { "type": "integer", "description": "Total segments across every aspect whose time window overlaps another segment in the same aspect." },
				"time_range": { "type": ["array", "null"], "items": { "type": "integer" }, "description": "Inclusive `[min, max]` timestamp span across the union of aspects, or `null`." }
			},
			"required": ["aspect_count", "segment_count", "total_rows", "total_nulls", "total_bytes", "bytes_per_point", "unsorted_segments", "overlapping_segments", "time_range"]
		}),
	);
	schemas.insert(
		"CatalogDatabase".to_string(),
		json!({
			"type": "object",
			"description": "One registered database in the `CatalogResponse`.",
			"properties": {
				"name": { "type": "string", "description": "The database name." },
				"subjects": { "type": "array", "items": { "type": "string" }, "description": "The subjects registered under this database, in name order." }
			},
			"required": ["name", "subjects"]
		}),
	);
	schemas.insert(
		"CatalogResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/catalog`.",
			"properties": {
				"database": { "type": "string", "description": "The database namespace this server's store is scoped to." },
				"subject": { "type": "string", "description": "The subject namespace this server's store is scoped to." },
				"databases": { "type": "array", "items": { "$ref": "#/components/schemas/CatalogDatabase" }, "description": "Every database registered in the store's catalog, each with its subjects." }
			},
			"required": ["database", "subject", "databases"]
		}),
	);
	schemas.insert(
		"HeadroomResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /api/v1/storage/headroom` (ROB-17).",
			"properties": {
				"total_bytes": { "type": "integer", "description": "The filesystem's size in bytes." },
				"available_bytes": { "type": "integer", "description": "The bytes an unprivileged writer can still use." },
				"reserve_bytes": { "type": "integer", "description": "The reserve every ingest or seal must leave (`WEFT_DISK_RESERVE_BYTES`; `0`: the guard is off)." },
				"writable_bytes": { "type": "integer", "description": "What a write may still take before the guard refuses it." },
				"refusing": { "type": "boolean", "description": "Whether the guard refuses every write now (`507 insufficient_storage`)." }
			},
			"required": ["total_bytes", "available_bytes", "reserve_bytes", "writable_bytes", "refusing"]
		}),
	);
}

/// Probes, version, and the telemetry profile shapes.
fn add_probe_version_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"HealthResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for the liveness probe (`GET /health`).",
			"properties": {
				"status": { "type": "string", "description": "Always `\"ok\"` while the process can serve a request." },
				"service": { "type": "string", "description": "Logical service name (`weft-server`)." },
				"version": { "type": "string", "description": "Running build version." }
			},
			"required": ["status", "service", "version"]
		}),
	);
	schemas.insert(
		"ReadyResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for the readiness probe (`GET /ready`).",
			"properties": {
				"ready": { "type": "boolean", "description": "`true` once the service can accept traffic; `false` — answered with `503` — exactly when the store reports `incompatible_store`." },
				"service": { "type": "string", "description": "Logical service name (`weft-server`)." },
				"version": { "type": "string", "description": "Running build version." },
				"segment_store": { "type": "boolean", "description": "Whether a segment store is configured — i.e. the storage endpoints are live rather than answering `503`." },
				"poisoned": { "type": "boolean", "description": "Whether the segment store is write-poisoned." },
				"restart_required": { "type": "boolean", "description": "Whether the server must be restarted to accept writes again." },
				"poison_reason": { "type": ["string", "null"], "description": "Why the store is poisoned, while it is." },
				"store_uuid": { "type": ["string", "null"], "description": "The store's identity (`store_uuid` of its `STORE_FORMAT` marker), when a store is configured." },
				"durability_class": { "type": ["string", "null"], "description": "The store's durability class (`full`, or `process-crash-only` on Windows), when a store is configured." },
				"previous_clean_shutdown": { "type": ["boolean", "null"], "description": "Whether the previous process that held the store shut down cleanly, when a store is configured." },
				"quarantined": { "type": "integer", "description": "Corrupt quarantine records the last recovery pass left, counted only." },
				"suspect": { "type": "integer", "description": "Suspect quarantine records the last recovery pass left, counted only." },
				"incompatible_store": { "type": "boolean", "description": "Whether any suspect record's cause is a frame version this build predates." },
				"disk_refusing": { "type": "boolean", "description": "Whether the store's disk guard is refusing every ingest and seal now." }
			},
			"required": ["ready", "service", "version", "segment_store", "poisoned", "restart_required", "quarantined", "suspect", "incompatible_store", "disk_refusing"]
		}),
	);
	schemas.insert(
		"BuildInfo".to_string(),
		json!({
			"type": "object",
			"description": "How the running binary was built.",
			"properties": {
				"os": { "type": "string", "description": "The operating system, e.g. `linux`." },
				"arch": { "type": "string", "description": "The CPU architecture, e.g. `x86_64`." },
				"family": { "type": "string", "description": "The platform family, `unix` or `windows`." },
				"env": { "type": "string", "description": "The target's environment qualifier, e.g. `gnu` or `musl`. Empty where the target carries no qualifier." },
				"target": { "type": "string", "description": "The full target triple, e.g. `x86_64-unknown-linux-gnu`." },
				"rustc": { "type": ["string", "null"], "description": "The compiler that produced the binary, e.g. `1.95.0`. Null when the build could not ask `rustc`." },
				"git_sha": { "type": ["string", "null"], "description": "The commit the binary was built from, short SHA. Null when the source tree had no git at build time." },
				"git_dirty": { "type": ["boolean", "null"], "description": "Whether that tree carried an uncommitted change. Null when `git_sha` is." },
				"pointer_width": { "type": "integer", "description": "Pointer width in bits (32 or 64)." },
				"debug_assertions": { "type": "boolean", "description": "Whether assertions are compiled in: a debug build, not a release one." },
				"profile": { "type": "string", "description": "The cargo profile the binary was built under — `release`, `debug`, `test`." }
			},
			"required": ["os", "arch", "family", "env", "target", "pointer_width", "debug_assertions", "profile"]
		}),
	);
	schemas.insert(
		"ScopeInfo".to_string(),
		json!({
			"type": "object",
			"description": "One half of a store scope: its database and subject.",
			"properties": {
				"database": { "type": "string", "description": "The database half." },
				"subject": { "type": "string", "description": "The subject half." }
			},
			"required": ["database", "subject"]
		}),
	);
	schemas.insert(
		"StoreIdentity".to_string(),
		json!({
			"type": "object",
			"description": "The store this process serves, or `{\"configured\": false}` when no store root is set.",
			"properties": {
				"configured": { "type": "boolean", "description": "Whether a store root is set and a store is open." },
				"store_uuid": { "type": ["string", "null"], "description": "The store's identity, the same UUID `/ready` reports." },
				"durability_class": { "type": ["string", "null"], "description": "What the store promises about durability." },
				"recorded_scope": { "type": ["object", "null"], "description": "The scope the store records.", "allOf": [{ "$ref": "#/components/schemas/ScopeInfo" }] },
				"layout_version": { "type": ["integer", "null"], "description": "The layout the store is in." },
				"min_read_layout": { "type": ["integer", "null"], "description": "The oldest layout a WeftDB must know to read this store." },
				"min_write_layout": { "type": ["integer", "null"], "description": "The oldest layout a WeftDB must know to write this store." },
				"last_written_layout": { "type": ["integer", "null"], "description": "The layout of the last WeftDB that wrote this store." },
				"supported_layout": { "type": "integer", "description": "The newest layout this build knows." }
			},
			"required": ["configured", "supported_layout"]
		}),
	);
}

/// Probes, version, and telemetry: the version response and the latency/interpolate/downsample/ingest profiles.
fn add_profile_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"ScopePinInfo".to_string(),
		json!({
			"type": "object",
			"description": "The scope pin as this process sees it.",
			"properties": {
				"mode": { "type": "string", "description": "`enforced` when a mismatched scope refuses startup, `off` when the pin is off." },
				"serving": { "type": ["object", "null"], "description": "The scope this process serves: null with no store configured.", "allOf": [{ "$ref": "#/components/schemas/ScopeInfo" }] },
				"recorded": { "type": ["object", "null"], "description": "The scope the store root's own marker records (null with no store configured).", "allOf": [{ "$ref": "#/components/schemas/ScopeInfo" }] },
				"matches_store": { "type": ["boolean", "null"], "description": "Whether the two agree: null with no store." }
			},
			"required": ["mode"]
		}),
	);
	schemas.insert(
		"ExitCodeInfo".to_string(),
		json!({
			"type": "object",
			"description": "One entry of the plan C7 exit-status table.",
			"properties": {
				"code": { "type": "integer", "description": "The process exit status." },
				"meaning": { "type": "string", "description": "The C7 meaning, in the table's own words." }
			},
			"required": ["code", "meaning"]
		}),
	);
	schemas.insert(
		"VersionResponse".to_string(),
		json!({
			"type": "object",
			"description": "The whole `GET /api/v1/version` body.",
			"properties": {
				"service": { "type": "string", "description": "Logical service name (`weft-server`)." },
				"version": { "type": "string", "description": "Running build version, the package version." },
				"build": { "$ref": "#/components/schemas/BuildInfo", "description": "How this binary was built." },
				"store": { "$ref": "#/components/schemas/StoreIdentity", "description": "The store this process serves, or `{\"configured\": false}` when no store root is set." },
				"scope_pin": { "$ref": "#/components/schemas/ScopePinInfo", "description": "The scope pin: what this process serves, what the store records, and whether the server refuses a mismatch." },
				"exit_codes": { "type": "array", "items": { "$ref": "#/components/schemas/ExitCodeInfo" }, "description": "The plan C7 exit-status table this process exits with." }
			},
			"required": ["service", "version", "build", "store", "scope_pin", "exit_codes"]
		}),
	);
	schemas.insert(
		"LatencyProfile".to_string(),
		json!({
			"type": "object",
			"description": "The estimated latency quantiles for one endpoint, in seconds.",
			"properties": {
				"count": { "type": "integer", "description": "Total observed requests (the histogram's `_count`)." },
				"sum_seconds": { "type": "number", "description": "Summed observed latency in seconds (the histogram's `_sum`)." },
				"p50_seconds": { "type": ["number", "null"], "description": "Estimated median (p50) latency in seconds." },
				"p95_seconds": { "type": ["number", "null"], "description": "Estimated p95 latency in seconds — the governing SLO term." },
				"p99_seconds": { "type": ["number", "null"], "description": "Estimated p99 latency in seconds." }
			},
			"required": ["count", "sum_seconds"]
		}),
	);
	schemas.insert(
		"InterpolateProfile".to_string(),
		json!({
			"type": "object",
			"description": "Per-endpoint counters plus latency quantiles for the interpolate endpoint.",
			"properties": {
				"requests": { "type": "integer", "description": "Total interpolation requests (including failures)." },
				"errors": { "type": "integer", "description": "Interpolation requests that returned an error." },
				"output_points": { "type": "integer", "description": "Total interpolated output points served." },
				"output_raw_points": { "type": "integer", "description": "Output points served with provenance `raw`." },
				"output_interpolated_points": { "type": "integer", "description": "Output points served with provenance `interpolated`." },
				"output_extrapolated_points": { "type": "integer", "description": "Output points served with provenance `extrapolated`." },
				"backend_cpu_requests": { "type": "integer", "description": "Successful requests the serial CPU backend produced." },
				"backend_parallel_requests": { "type": "integer", "description": "Successful requests the rayon CPU pool produced." },
				"backend_gpu_requests": { "type": "integer", "description": "Successful requests the GPU produced." },
				"gpu_fallbacks": { "type": "integer", "description": "Successful requests where `auto` tried the GPU and fell back." },
				"latency_seconds": { "$ref": "#/components/schemas/LatencyProfile", "description": "End-to-end latency quantiles for `POST /api/v1/interpolate`." }
			},
			"required": ["requests", "errors", "output_points", "output_raw_points", "output_interpolated_points", "output_extrapolated_points", "backend_cpu_requests", "backend_parallel_requests", "backend_gpu_requests", "gpu_fallbacks", "latency_seconds"]
		}),
	);
}

/// Probes, version, and telemetry: the downsample/ingest profiles and the profile report.
fn add_downsample_profile_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"DownsampleProfile".to_string(),
		json!({
			"type": "object",
			"description": "Per-endpoint counters plus latency quantiles for the downsample endpoint.",
			"properties": {
				"requests": { "type": "integer", "description": "Total downsample requests (including failures)." },
				"errors": { "type": "integer", "description": "Downsample requests that returned an error." },
				"output_buckets": { "type": "integer", "description": "Total non-empty buckets served." },
				"latency_seconds": { "$ref": "#/components/schemas/LatencyProfile", "description": "End-to-end latency quantiles for `POST /api/v1/downsample`." }
			},
			"required": ["requests", "errors", "output_buckets", "latency_seconds"]
		}),
	);
	schemas.insert(
		"IngestProfile".to_string(),
		json!({
			"type": "object",
			"description": "Storage-ingest counters plus the seal-path latency quantiles.",
			"properties": {
				"requests": { "type": "integer", "description": "Total storage-ingest requests (including failures)." },
				"errors": { "type": "integer", "description": "Storage-ingest requests that returned an error." },
				"rows_sealed": { "type": "integer", "description": "Total rows sealed across all successful ingests." },
				"segments_sealed": { "type": "integer", "description": "Total segments sealed across all successful ingests." },
				"latency_seconds": { "$ref": "#/components/schemas/LatencyProfile", "description": "Seal-path latency quantiles for the storage-ingest endpoints." }
			},
			"required": ["requests", "errors", "rows_sealed", "segments_sealed", "latency_seconds"]
		}),
	);
	schemas.insert(
		"ProfileReport".to_string(),
		json!({
			"type": "object",
			"description": "The live counter + latency-quantile snapshot (`GET /debug/profile/current`).",
			"properties": {
				"service": { "type": "string", "description": "Logical service name." },
				"version": { "type": "string", "description": "Running build version." },
				"interpolate": { "$ref": "#/components/schemas/InterpolateProfile", "description": "Interpolation-endpoint counters + latency quantiles." },
				"downsample": { "$ref": "#/components/schemas/DownsampleProfile", "description": "Downsample-endpoint counters + latency quantiles." },
				"ingest": { "$ref": "#/components/schemas/IngestProfile", "description": "Storage-ingest counters + seal-path latency quantiles." }
			},
			"required": ["service", "version", "interpolate", "downsample", "ingest"]
		}),
	);
}

/// Ingest and declaration: point batches and aspect declarations.
fn add_ingest_declare_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"IngestPoint".to_string(),
		json!({
			"type": "object",
			"description": "One point in an ingest batch.",
			"properties": {
				"timestamp": { "type": "integer", "description": "Row timestamp (epoch integer in the aspect's declared unit)." },
				"value": { "type": ["string", "null"], "description": "The value as decimal text (no float round-trip), or `null` for a null row." }
			},
			"required": ["timestamp"]
		}),
	);
	schemas.insert(
		"IngestRequest".to_string(),
		json!({
			"type": "object",
			"description": "Request body for `POST /api/v1/storage/{aspect}/points` — seal a batch of points.",
			"properties": {
				"points": { "type": "array", "items": { "$ref": "#/components/schemas/IngestPoint" }, "description": "The points to seal, in caller order (the seal records whether they are time-sorted)." },
				"rows_per_page": { "type": ["integer", "null"], "description": "Optional page height — when present, seal a paged segment of this many rows per page." },
				"require_sorted": { "type": "boolean", "description": "When `true`, reject the batch (`400`) if its timestamps are not monotonic non-decreasing. Defaults to `false`." },
				"batch_id": { "type": ["string", "null"], "description": "The caller's idempotency key, as the body's own spelling of the `Idempotency-Key` header. The header wins when both are sent." },
				"tags": { "type": ["object", "null"], "description": "The tags naming the one series the batch writes, as a JSON object of string values. Absent, `null` or `{}` is the untagged series 0.", "additionalProperties": { "type": "string" } }
			},
			"required": ["points"]
		}),
	);
	schemas.insert(
		"IngestResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for a successful ingest: the sealed segment's descriptor summary.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect the batch was sealed into." },
				"segment_id": { "type": "integer", "description": "The id assigned to the new segment (monotonic within the aspect)." },
				"format_version": { "type": "integer", "description": "The `.weftseg` frame format version sealed (5 single-block, 6 paged)." },
				"row_count": { "type": "integer", "description": "Total rows sealed (present and null)." },
				"null_count": { "type": "integer", "description": "Null rows sealed." },
				"byte_len": { "type": "integer", "description": "The realized on-disk frame size in bytes (the bytes/point numerator)." },
				"min_ts": { "type": ["integer", "null"], "description": "Smallest timestamp in the sealed segment, or `null` if it was empty." },
				"max_ts": { "type": ["integer", "null"], "description": "Largest timestamp in the sealed segment, or `null` if it was empty." },
				"time_sorted": { "type": "boolean", "description": "Whether the sealed segment's timestamps are monotonic non-decreasing." },
				"segment_ids": { "type": "array", "items": { "type": "integer" }, "description": "Every segment id the batch's commit assigned, in time order." },
				"series_id": { "type": "integer", "description": "The id of the series the batch wrote: `0` for the untagged series." },
				"tags": { "type": "object", "description": "The tags of the series the batch wrote: `{}` for the untagged series 0.", "additionalProperties": { "type": "string" } },
				"replayed": { "type": "boolean", "description": "Whether this response is a replay: the request's idempotency key was already committed with the same batch, so nothing was written again." }
			},
			"required": ["aspect", "segment_id", "format_version", "row_count", "null_count", "byte_len", "time_sorted", "segment_ids", "series_id", "tags", "replayed"]
		}),
	);
	schemas.insert(
		"DeclareAspectRequest".to_string(),
		json!({
			"type": "object",
			"description": "Request body for `POST /api/v1/storage/aspects` — declare an aspect's schema.",
			"properties": {
				"name": { "type": "string", "description": "The aspect name to declare (unique within the store's `(database, subject)` scope). At most 160 bytes, no `/`, `\\`, control characters, leading `.`, trailing `.` or space, not a Windows device name." },
				"physical_type": { "type": "string", "description": "The physical encoding token (e.g. `\"f64\"`, `\"scaled_i64\"`)." },
				"scale": { "type": ["integer", "null"], "description": "Fractional-decimal-digit scale, required for `scaled_i64` / `scaled_i128`." },
				"value_tolerance": { "type": ["string", "null"], "description": "The permitted per-value reconstruction error as decimal text. Defaults to `\"0\"` (the encoding must be exact for every value)." },
				"timestamp_unit": { "type": "string", "description": "The timestamp resolution token (e.g. `\"seconds\"`, `\"millis\"`)." },
				"upsert_keys": { "type": ["array", "null"], "items": { "type": "string" }, "description": "The aspect's upsert keys: `\"timestamp\"` exactly once, plus the tag keys every ingested row's tags must fall inside. Omitted, the aspect declares none." }
			},
			"required": ["name", "physical_type", "timestamp_unit"]
		}),
	);
	schemas.insert(
		"DeclareAspectResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/aspects` — the declared schema read back.",
			"properties": { "aspect": { "$ref": "#/components/schemas/AspectInfo", "description": "The declared aspect." } },
			"required": ["aspect"]
		}),
	);
}

/// Maintenance and backup: sweep/reconcile/squash/compact outcomes.
fn add_maintenance_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"SweepFailure".to_string(),
		json!({
			"type": "object",
			"description": "One aspect whose sweep pass failed.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect whose pass failed." },
				"error": { "type": "string", "description": "The failure with its full cause chain." }
			},
			"required": ["aspect", "error"]
		}),
	);
	schemas.insert(
		"ReconcileResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/{aspect}/reconcile`.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect reconciled." },
				"mode": { "type": "string", "description": "The sweep mode applied — `\"threshold\"` (all-or-nothing), `\"hot_cold\"` or `\"overlaps\"`." },
				"triggered": { "type": "boolean", "description": "Whether the pass actually ran. Always `true` for an unconditional or hot/cold call; `false` when a `threshold` was given and the backlog held below it." },
				"reconciled": { "type": "integer", "description": "Number of out-of-order segments rewritten sorted by this call (0 when the aspect was already fully ordered, or when the pass did not trigger)." },
				"cold_reconciled": { "type": "integer", "description": "Cold (non-hot-tail) segments rewritten (hot/cold mode only; 0 in threshold mode)." },
				"hot_reconciled": { "type": "integer", "description": "Hot-tail segments rewritten (hot/cold mode only; 0 in threshold mode)." },
				"unsorted_segments": { "type": "integer", "description": "The intra-segment order-health count after the pass — zero once every segment admits ordered (binary-search) access." },
				"overlapping_segments": { "type": "integer", "description": "The cross-segment overlap count after the pass — zero once no two segments' time windows intersect." }
			},
			"required": ["aspect", "mode", "triggered", "reconciled", "cold_reconciled", "hot_reconciled", "unsorted_segments", "overlapping_segments"]
		}),
	);
	schemas.insert(
		"ReconcileStoreResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/reconcile` — the outcome of a sweep over every aspect.",
			"properties": {
				"threshold": { "type": "integer", "description": "The order-health backlog threshold applied (an absent `?threshold=` defaults to 1)." },
				"mode": { "type": "string", "description": "The sweep mode applied — `\"threshold\"` (all-or-nothing) or `\"hot_cold\"`." },
				"aspects_scanned": { "type": "integer", "description": "Number of declared aspects the sweep visited." },
				"aspects_reconciled": { "type": "integer", "description": "Number of aspects that were reconciled." },
				"segments_reconciled": { "type": "integer", "description": "Total out-of-order segments rewritten sorted across the sweep." },
				"cold_reconciled": { "type": "integer", "description": "Total cold (non-hot-tail) segments rewritten (hot/cold mode only; 0 in threshold mode)." },
				"hot_reconciled": { "type": "integer", "description": "Total hot-tail segments rewritten (hot/cold mode only; 0 in threshold mode)." },
				"unsorted_segments": { "type": "integer", "description": "The store-wide order-health count after the sweep." },
				"overlapping_segments": { "type": "integer", "description": "The store-wide cross-segment overlap count after the sweep." },
				"failed": { "type": "array", "items": { "$ref": "#/components/schemas/SweepFailure" }, "description": "The aspects whose pass failed, each `{aspect, error}`, in the order the sweep visited them. Empty when every aspect succeeded — a caller must check this list to tell a partial sweep from a clean one." },
				"tripped": { "type": "array", "items": { "type": "string" }, "description": "The aspects the sweep did not attempt because their per-aspect maintenance circuit breaker is tripped. Empty when no breaker is tripped." }
			},
			"required": ["threshold", "mode", "aspects_scanned", "aspects_reconciled", "segments_reconciled", "cold_reconciled", "hot_reconciled", "unsorted_segments", "overlapping_segments", "failed", "tripped"]
		}),
	);
	schemas.insert(
		"SquashResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/{aspect}/squash`.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect squashed." },
				"triggered": { "type": "boolean", "description": "Whether the squash actually ran. `false` only when a `max_segments` cap was given and the aspect's segment count held at or below it." },
				"removed": { "type": "integer", "description": "Number of segments removed by folding the aspect into one (0 when it already had fewer than two segments, or the pass did not trigger)." },
				"segment_count": { "type": "integer", "description": "The aspect's segment count after the pass — 1 once a squash has folded it." }
			},
			"required": ["aspect", "triggered", "removed", "segment_count"]
		}),
	);
	schemas.insert(
		"CompactResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/{aspect}/compact`.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect compacted." },
				"removed": { "type": "integer", "description": "Number of segments removed by coalescing groups toward the target size (0 when nothing coalesced)." },
				"segment_count": { "type": "integer", "description": "The aspect's segment count after the pass." }
			},
			"required": ["aspect", "removed", "segment_count"]
		}),
	);
}

/// Maintenance and backup: recovery, quarantine, and purge outcomes.
fn add_recovery_quarantine_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"RecoveryReport".to_string(),
		json!({
			"type": "object",
			"description": "What one recovery pass — at startup, or behind the fsck endpoint — found and did. Counts only, so `/ready` can carry it without leaking segment contents.",
			"properties": {
				"checked": { "type": "integer", "description": "Index rows examined." },
				"quarantined": { "type": "integer", "description": "Rows moved to corrupt quarantine." },
				"suspected": { "type": "integer", "description": "Rows moved to suspect quarantine." },
				"rebound": { "type": "integer", "description": "Rows re-bound to their frame's key." },
				"seeded": { "type": "integer", "description": "`aspect_seq.next_series_id` rows seeded." },
				"purged": { "type": "integer", "description": "Quarantine records purged by this pass (TTL expiry)." },
				"orphans": { "type": "integer", "description": "Files under `segments/` no index row or journal entry claims." },
				"adopted": { "type": "integer", "description": "Orphan frames a restore-mode pass adopted into the index, plus held frames a later pass released into it." },
				"held": { "type": "integer", "description": "Orphan frames a restore-mode pass journaled `held`: they carry a series key and their aspect has no declared schema yet." },
				"series_rebound": { "type": "integer", "description": "Rows whose series id was rebound to the series their frame's key names." },
				"held_cleared": { "type": "integer", "description": "Held journal rows cleared without an adoption." },
				"skipped_outside_segments": { "type": "integer", "description": "Rows whose indexed path escapes `segments/` and were left exactly where they are." },
				"incompatible_store": { "type": "boolean", "description": "Whether any suspect record's cause is a frame version this build predates: `/ready` reports `incompatible_store` exactly then." }
			},
			"required": ["checked", "quarantined", "suspected", "rebound", "seeded", "purged", "orphans", "adopted", "held", "series_rebound", "held_cleared", "skipped_outside_segments", "incompatible_store"]
		}),
	);
}

/// Maintenance and backup: quarantine records and purge outcomes.
fn add_quarantine_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"QuarantineRecord".to_string(),
		json!({
			"type": "object",
			"description": "One quarantined segment.",
			"properties": {
				"aspect": { "type": "string", "description": "The aspect the segment belonged to." },
				"id": { "type": "integer", "description": "The segment id its index row carried." },
				"gen": { "type": "integer", "description": "The segment generation its index row carried." },
				"name": { "type": "string", "description": "The file name under `segments/quarantine/` (or the indexed name, when the file itself was missing)." },
				"class": { "type": "string", "enum": ["corrupt", "suspect"], "description": "The C5 class: `corrupt` is an integrity failure (TTL purge applies); `suspect` is data another build can read (no automatic purge, ever)." },
				"reason": { "type": "string", "description": "Why it was quarantined, in the decoder's or the scanner's words." },
				"min_ts": { "type": ["integer", "null"], "description": "The segment's time span, from its index row." },
				"max_ts": { "type": ["integer", "null"], "description": "The segment's time span, from its index row." },
				"series_id": { "type": "integer", "description": "The tag series the segment's frame holds (0 is the empty tag set)." },
				"quarantined_ms": { "type": "integer", "description": "Milliseconds since the Unix epoch when the record was written; the TTL counts from here." },
				"descriptor_json": { "type": ["string", "null"], "description": "The quarantined row itself, serialized, so an operator release can put the segment back exactly as it was. `None` for records with no row behind them." }
			},
			"required": ["aspect", "id", "gen", "name", "class", "reason", "series_id", "quarantined_ms"]
		}),
	);
	schemas.insert(
		"QuarantineRecords".to_string(),
		json!({
			"type": "array",
			"description": "The quarantine records the recovery pass left.",
			"items": { "$ref": "#/components/schemas/QuarantineRecord" }
		}),
	);
	schemas.insert(
		"PurgeExpiredResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/quarantine/purge-expired`.",
			"properties": { "purged": { "type": "integer", "description": "How many expired quarantine records were purged." } },
			"required": ["purged"]
		}),
	);
	schemas.insert(
		"BackupDbReport".to_string(),
		json!({
			"type": "object",
			"description": "Per-database verification of one restored or snapshotted database.",
			"properties": {
				"name": { "type": "string", "description": "The database's file name." },
				"tables": { "type": "integer", "description": "Tables verified." },
				"rows": { "type": "integer", "description": "Rows verified readable." },
				"bytes": { "type": "integer", "description": "On-disk size of the restored files in bytes." }
			},
			"required": ["name", "tables", "rows", "bytes"]
		}),
	);
}

/// Maintenance and backup: backup and restore-drill responses.
fn add_backup_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"BackupResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/backup`.",
			"properties": {
				"dir": { "type": "string", "description": "The snapshot directory (the label, under the backup root)." },
				"databases": { "type": "array", "items": { "$ref": "#/components/schemas/BackupDbReport" }, "description": "Per-database verification of each snapshotted file." },
				"total_rows": { "type": "integer", "description": "Total rows verified readable across the snapshotted control plane." },
				"total_bytes": { "type": "integer", "description": "Total on-disk size of the snapshotted files in bytes." },
				"verify": { "type": "string", "description": "How the copy was verified: `source` (quiescent cross-check) or `snapshot` (copy-only check)." },
				"duration_ms": { "type": "integer", "description": "Wall-clock milliseconds the snapshot took." }
			},
			"required": ["dir", "databases", "total_rows", "total_bytes", "verify", "duration_ms"]
		}),
	);
	schemas.insert(
		"RestoreDrillResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/restore/drill` — the outcome of a rehearsed restore.",
			"properties": {
				"label": { "type": "string", "description": "The backup directory that was rehearsed." },
				"databases": { "type": "array", "items": { "$ref": "#/components/schemas/BackupDbReport" }, "description": "Per-database verification of each restored file, read back at its restore destination." },
				"total_rows": { "type": "integer", "description": "Total rows verified readable across the restored control plane." },
				"total_bytes": { "type": "integer", "description": "Total on-disk size of the restored files in bytes." },
				"duration_ms": { "type": "integer", "description": "Wall-clock milliseconds the rehearsal restore itself took. Cleanup of the rehearsal copy is excluded." },
				"restorable": { "type": "boolean", "description": "Always `true` when the call returns `200`: the backup restored and every restored database opened and scanned. A failure is reported as a non-`200` instead." },
				"cleanup_error": { "type": ["string", "null"], "description": "Why the rehearsal copy could not be removed afterwards, or `null` when it was." }
			},
			"required": ["label", "databases", "total_rows", "total_bytes", "duration_ms", "restorable"]
		}),
	);
	schemas.insert(
		"RestoreIntoResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `POST /api/v1/storage/restore/into` — the outcome of restoring a backup into a caller-chosen new root.",
			"properties": {
				"backup_label": { "type": "string", "description": "The backup label that was restored." },
				"restored_root": { "type": "string", "description": "The absolute path of the restored root directory." },
				"mode": { "type": "string", "description": "`\"whole_store\"` or `\"control_plane\"`, chosen by the backup's manifest." },
				"databases": { "type": "array", "items": { "$ref": "#/components/schemas/BackupDbReport" }, "description": "Per-database verification of each restored file, read back at its destination." },
				"rows_verified": { "type": "integer", "description": "Total rows verified readable across the restored control plane." },
				"bytes": { "type": "integer", "description": "Total on-disk size of the restored files in bytes." },
				"elapsed_ms": { "type": "integer", "description": "Wall-clock milliseconds the restore itself took." },
				"next_step": { "type": "string", "description": "What the operator should do next to serve the restored store." }
			},
			"required": ["backup_label", "restored_root", "mode", "databases", "rows_verified", "bytes", "elapsed_ms", "next_step"]
		}),
	);
}

/// Benchmark runs and Grafana: run manifests and the datasource protocol shapes.
fn add_bench_grafana_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"RunSummary".to_string(),
		json!({
			"type": "object",
			"description": "One benchmark run manifest's identity, as `GET /bench/runs` lists it.",
			"properties": {
				"id": { "type": "string", "description": "The run id: SHA-256 of the manifest's bytes, lower-case hex." },
				"path": { "type": "string", "description": "The manifest's path relative to the run directory, `/`-separated." },
				"schema_version": { "type": ["integer", "null"], "description": "The manifest's `schema_version`, when it has one." },
				"generated_at": { "type": ["string", "null"], "description": "When the run finished (the manifest's `generated_at`)." },
				"claim": { "type": ["string", "null"], "description": "The `--claim` the run is evidence for, if labelled as one." }
			},
			"required": ["id", "path"]
		}),
	);
	schemas.insert(
		"RunListResponse".to_string(),
		json!({
			"type": "object",
			"description": "Response body for `GET /bench/runs`.",
			"properties": { "runs": { "type": "array", "items": { "$ref": "#/components/schemas/RunSummary" }, "description": "Every run found, newest `generated_at` first." } },
			"required": ["runs"]
		}),
	);
	schemas.insert(
		"RunManifest".to_string(),
		json!({
			"type": "object",
			"description": "One benchmark run's manifest JSON, as the run wrote it (content-addressed by its SHA-256 id). The manifest's own `schema_version` names its shape."
		}),
	);
	schemas.insert(
		"GrafanaTest".to_string(),
		json!({
			"type": "object",
			"description": "The Grafana connection test's answer.",
			"properties": {
				"status": { "type": "string", "description": "Always `\"ok\"`." },
				"datasource": { "type": "string", "description": "Always `\"weftdb\"`." }
			},
			"required": ["status", "datasource"]
		}),
	);
	schemas.insert(
		"GrafanaFind".to_string(),
		json!({
			"type": "object",
			"description": "Request body for `POST /api/v1/grafana/metrics`: the picker's typed prefix, if it sent one.",
			"properties": { "metric": { "type": ["string", "null"], "description": "The typed prefix to filter aspects by (case-insensitive substring). Absent, every aspect is listed." } }
		}),
	);
}

/// Grafana: the datasource protocol shapes.
fn add_grafana_schemas(schemas: &mut Map<String, Value>) {
	schemas.insert(
		"MetricOption".to_string(),
		json!({
			"type": "object",
			"description": "One entry of the Grafana metric picker.",
			"properties": {
				"label": { "type": "string", "description": "What the picker shows." },
				"value": { "type": "string", "description": "What a target carries." },
				"payloads": { "type": "array", "items": true, "description": "The per-target options (the `agg` reduction picker)." }
			},
			"required": ["label", "value", "payloads"]
		}),
	);
	schemas.insert(
		"MetricOptions".to_string(),
		json!({
			"type": "array",
			"description": "The metric picker's options.",
			"items": { "$ref": "#/components/schemas/MetricOption" }
		}),
	);
	schemas.insert(
		"GrafanaRange".to_string(),
		json!({
			"type": "object",
			"description": "The time range of a Grafana query.",
			"properties": {
				"from": { "type": "string", "format": "date-time", "description": "The first instant." },
				"to": { "type": "string", "format": "date-time", "description": "The last instant." }
			},
			"required": ["from", "to"]
		}),
	);
	schemas.insert(
		"GrafanaTarget".to_string(),
		json!({
			"type": "object",
			"description": "One target of a Grafana query.",
			"properties": {
				"target": { "type": ["string", "null"], "description": "The aspect." },
				"refId": { "type": ["string", "null"], "description": "The panel's id for this target." },
				"hide": { "type": "boolean", "description": "A hidden target is not queried." },
				"payload": { "type": ["object", "null"], "description": "The per-target options (`agg`)." }
			}
		}),
	);
	schemas.insert(
		"GrafanaQuery".to_string(),
		json!({
			"type": "object",
			"description": "Request body for `POST /api/v1/grafana/query`: the fields this server reads; Grafana sends more.",
			"properties": {
				"range": { "$ref": "#/components/schemas/GrafanaRange", "description": "The panel's time range." },
				"intervalMs": { "type": ["integer", "null"], "description": "The panel's interval in milliseconds." },
				"maxDataPoints": { "type": ["integer", "null"], "description": "The most points the panel can draw." },
				"targets": { "type": "array", "items": { "$ref": "#/components/schemas/GrafanaTarget" }, "description": "The series to draw." }
			},
			"required": ["range"]
		}),
	);
	schemas.insert(
		"GrafanaSeries".to_string(),
		json!({
			"type": "object",
			"description": "One answered Grafana series.",
			"properties": {
				"target": { "type": "string", "description": "The aspect it reads." },
				"refId": { "type": ["string", "null"], "description": "The panel's id for it." },
				"datapoints": { "type": "array", "description": "`[value, epoch_ms]` pairs, oldest first.", "prefixItems": [{ "type": "number" }, { "type": "integer" }] }
			},
			"required": ["target", "datapoints"]
		}),
	);
	schemas.insert(
		"GrafanaSeriesList".to_string(),
		json!({
			"type": "array",
			"description": "One answered series per visible Grafana target.",
			"items": { "$ref": "#/components/schemas/GrafanaSeries" }
		}),
	);
}

fn build_components() -> Value {
	let mut schemas: Map<String, Value> = Map::new();
	schemas.insert("ErrorEnvelope".to_string(), error_envelope_schema());
	schemas.insert("PointsPage".to_string(), points_page_schema());
	schemas.insert("StoredPoint".to_string(), stored_point_schema());
	add_compute_request_schemas(&mut schemas);
	add_compute_downsample_schemas(&mut schemas);
	add_downsample_schemas(&mut schemas);
	add_stored_read_schemas(&mut schemas);
	add_aspect_read_schemas(&mut schemas);
	add_catalog_headroom_schemas(&mut schemas);
	add_probe_version_schemas(&mut schemas);
	add_profile_schemas(&mut schemas);
	add_downsample_profile_schemas(&mut schemas);
	add_ingest_declare_schemas(&mut schemas);
	add_maintenance_schemas(&mut schemas);
	add_recovery_quarantine_schemas(&mut schemas);
	add_quarantine_schemas(&mut schemas);
	add_backup_schemas(&mut schemas);
	add_bench_grafana_schemas(&mut schemas);
	add_grafana_schemas(&mut schemas);

	let mut responses: Map<String, Value> = Map::new();
	for &status in ERROR_STATUSES {
		let key = format!("Error{status}");
		responses.insert(key, error_response_entry(status));
	}

	let mut security_schemes: Map<String, Value> = Map::new();
	security_schemes.insert(
		"BearerAuth".to_string(),
		json!({
			"type": "http",
			"scheme": "bearer",
			"description": "An API key minted by `weft-server keygen`, sent as `Authorization: Bearer <key>` (SEC-6). With authentication on, every route except the probes requires it; the probes answer anonymously.",
		}),
	);

	json!({
		"schemas": schemas,
		"responses": responses,
		"securitySchemes": security_schemes,
	})
}

/// Generate the full `OpenAPI` 3.1 document from a sealed route table.
///
/// Every path item, operation, path parameter, scope and admission class
/// comes from the same registrations the running server uses.
#[must_use]
pub fn from_table(table: &RouteTable) -> Value {
	let mut path_groups: BTreeMap<&'static str, Map<String, Value>> = BTreeMap::new();
	for entry in table.entries() {
		let methods = path_groups.entry(entry.path).or_default();
		let operation = build_operation(&entry.spec);
		methods.insert(method_key(&entry.method).to_string(), operation);
	}

	// A BTreeMap iterates in key order, so the document is deterministic
	// without sorting the paths a second time.
	let mut path_items: Map<String, Value> = Map::new();
	for (path, methods) in &path_groups {
		let mut item: Map<String, Value> = Map::new();
		for (method, operation) in methods {
			item.insert(method.clone(), operation.clone());
		}
		// Inject path parameters from the template.
		let params = extract_path_parameters(path);
		if !params.is_empty() {
			item.insert("parameters".to_string(), Value::Array(params));
		}
		path_items.insert((*path).to_string(), Value::Object(item));
	}

	json!({
		"openapi": "3.1.0",
		"info": {
			"title": crate::SERVICE,
			"version": crate::VERSION,
			"description": "The weft-server HTTP API, generated from the route registry. This is the contract candidate for the 1.0 beta (FRE-9b): every operation documents its parameters, request body and success response. With authentication on (`WEFT_AUTH=keys`), every route except the probes requires `Authorization: Bearer <key>`; the probes answer anonymously. With authentication off, every route answers anonymously.",
		},
		"servers": [
			{
				"url": "http://127.0.0.1:8080",
				"description": "The default bind address (`WEFT_SERVER_ADDR` overrides).",
			},
		],
		"paths": path_items,
		"components": build_components(),
	})
}

// ── structural validation ──────────────────────────────────────────────────

/// Validate the structural integrity of an `OpenAPI` document.
///
/// Returns a list of issues found: an empty list means the document
/// passes all checks.
#[must_use]
pub fn validate(document: &Value) -> Vec<String> {
	let mut issues: Vec<String> = Vec::new();
	// A missing section is something to report, not something to panic on, so
	// every `as_object` below borrows this map instead of a temporary.
	let empty = Map::new();

	// 1. OpenAPI version is 3.1.x
	let version = document["openapi"].as_str().unwrap_or("");
	if !version.starts_with("3.1") {
		issues.push(format!("`openapi` version must start with `3.1`, got {version:?}"));
	}

	let components = document["components"].as_object().unwrap_or(&empty);
	let schemas = components["schemas"].as_object().unwrap_or(&empty);
	let comp_responses = components["responses"].as_object().unwrap_or(&empty);

	let paths = document["paths"].as_object().unwrap_or(&empty);
	let mut operation_ids: Vec<String> = Vec::new();

	for (ppath, item) in paths {
		let Some(methods) = item.as_object() else {
			issues.push(format!("path `{ppath}` is not an object"));
			continue;
		};
		let method_keys: Vec<&str> = methods.keys().map(String::as_str).filter(|key| *key != "parameters").collect();

		if method_keys.is_empty() {
			issues.push(format!("path `{ppath}` has no operations"));
			continue;
		}

		// Path-item parameters cover every `{name}` in the template (FRE-9b:
		// a path parameter the document never declares is a contract hole).
		let item_params = methods.get("parameters").and_then(Value::as_array);
		let declared: Vec<&str> = item_params.map_or_else(Vec::new, |params| params.iter().filter_map(|param| param["name"].as_str()).collect());
		for segment in ppath.split('/') {
			if segment.starts_with('{') && segment.ends_with('}') {
				let name = &segment[1..segment.len() - 1];
				if !declared.contains(&name) {
					issues.push(format!("path `{ppath}` declares no parameter for `{{{name}}}`"));
				}
			}
		}

		for method in method_keys {
			let Some(operation) = methods[method].as_object() else {
				issues.push(format!("`{ppath}`/{method} is not an object"));
				continue;
			};

			// 3a. Has operationId.
			let op_id = operation["operationId"].as_str().unwrap_or("");
			if op_id.is_empty() {
				issues.push(format!("`{ppath}`/{method} has no `operationId`"));
			} else {
				operation_ids.push(op_id.to_string());
			}

			// 3b. Has summary.
			if operation["summary"].as_str().unwrap_or("").is_empty() {
				issues.push(format!("`{ppath}`/{method} has no `summary`"));
			}

			// 3b2. FRE-9b: has a description — a summary derived from the
			// operation id is not a contract.
			if operation["description"].as_str().unwrap_or("").is_empty() {
				issues.push(format!("`{ppath}`/{method} has no `description`"));
			}

			// 3b3. FRE-9b: `security` is stated on every operation — `[]` for
			// the anonymous probes, `BearerAuth` for the rest (SEC-6).
			match operation.get("security") {
				Some(Value::Array(entries)) => {
					let bearer = entries.iter().any(|entry| entry.get("BearerAuth").is_some());
					if !entries.is_empty() && !bearer {
						issues.push(format!("`{ppath}`/{method} has a `security` entry that is not `BearerAuth`"));
					}
				}
				_ => issues.push(format!("`{ppath}`/{method} has no `security`")),
			}

			// 3b4. FRE-9b: every declared parameter names its location and schema.
			if let Some(Value::Array(params)) = operation.get("parameters") {
				for param in params {
					let name = param["name"].as_str().unwrap_or("");
					if name.is_empty() || param["in"].as_str().unwrap_or("").is_empty() || !param["schema"].is_object() {
						issues.push(format!("`{ppath}`/{method} has a parameter without a name, location or schema"));
					}
				}
			}

			// 3c. Has a 2xx response.
			let responses = operation["responses"].as_object().unwrap_or(&empty);
			let has_2xx = responses.keys().any(|k| k.starts_with('2'));
			if !has_2xx {
				issues.push(format!("`{ppath}`/{method} has no `2xx` response"));
			}

			// 3c2. FRE-9b: every 2xx response carries content — a bare
			// description is not a response schema.
			for (code, response) in responses {
				if code.starts_with('2') && !response.get("content").is_some_and(Value::is_object) {
					issues.push(format!("`{ppath}`/{method} `{code}` response has no `content`"));
				}
			}

			// 3d. Has at least one documented error response.
			let has_error = responses.keys().any(|k| k.starts_with('4') || k.starts_with('5'));
			if !has_error {
				issues.push(format!("`{ppath}`/{method} has no documented error response"));
			}
		}
	}

	// 4. Every $ref target exists in components.
	collect_refs(document, &mut issues, schemas, comp_responses);

	// 5. No two operations share an operationId.
	operation_ids.sort();
	for i in 1..operation_ids.len() {
		if operation_ids[i] == operation_ids[i - 1] {
			issues.push(format!("duplicate `operationId`: `{}`", operation_ids[i]));
		}
	}

	issues
}

/// Collect every `$ref` in the document and check that its target exists.
fn collect_refs(value: &Value, issues: &mut Vec<String>, schemas: &Map<String, Value>, responses: &Map<String, Value>) {
	match value {
		Value::Object(obj) => {
			for (key, val) in obj {
				if key == "$ref" {
					if let Some(ref_str) = val.as_str() {
						check_ref(ref_str, issues, schemas, responses);
					}
				} else {
					collect_refs(val, issues, schemas, responses);
				}
			}
		}
		Value::Array(arr) => {
			for val in arr {
				collect_refs(val, issues, schemas, responses);
			}
		}
		_ => {}
	}
}

/// Check that a single `$ref` string targets an existing component.
fn check_ref(target: &str, issues: &mut Vec<String>, schemas: &Map<String, Value>, responses: &Map<String, Value>) {
	if !target.starts_with("#/components/") {
		issues.push(format!("`{target}` is not a valid `$ref` (must start with `#/components/`)"));
		return;
	}
	let tail = target.strip_prefix("#/components/").unwrap_or("");
	// tail is "schemas/Foo" or "responses/Error400"
	let Some((category, name)) = tail.split_once('/') else {
		issues.push(format!("`{target}` has an unrecognised component path"));
		return;
	};
	if category.is_empty() || name.is_empty() {
		issues.push(format!("`{target}` has an unrecognised component path"));
		return;
	}
	match category {
		"schemas" => {
			if !schemas.contains_key(name) {
				issues.push(format!("`$ref` target `{name}` not found in `components/schemas`"));
			}
		}
		"responses" => {
			if !responses.contains_key(name) {
				issues.push(format!("`$ref` target `{name}` not found in `components/responses`"));
			}
		}
		_ => {
			issues.push(format!("`$ref` references unknown component category `{category}`"));
		}
	}
}

// ── CLI subcommand ─────────────────────────────────────────────────────────

/// Run the `openapi` subcommand: generate the document and write it.
///
/// `arguments` follows the program name, starting with the subcommand word
/// `openapi`. Returns a usage error on a parse refusal, or propagates an
/// I/O error.
///
/// # Errors
///
/// Returns a usage error when the arguments name an unknown flag, repeat a
/// flag, or give `--out` no path. Returns an I/O error when the document
/// cannot be serialised, or the target path or stdout cannot be written.
pub fn run(arguments: Vec<String>) -> anyhow::Result<()> {
	use std::io::Write as _;
	let out_path = parse_args(arguments)?;

	let table = crate::routes::build_registry().table();
	let document = from_table(&table);

	// Pretty-printed with a trailing newline: the committed document is diffed
	// by hand as well as by the drift gate.
	let mut json_bytes = serde_json::to_vec_pretty(&document)?;
	json_bytes.push(b'\n');

	match out_path.as_deref() {
		None | Some("-") => {
			std::io::stdout().write_all(&json_bytes).map_err(|error| anyhow::anyhow!("openapi: writing the document to stdout failed: {error}"))?;
		}
		Some(path) => {
			std::fs::write(path, &json_bytes).map_err(|error| anyhow::anyhow!("openapi: writing the document to {path} failed: {error}"))?;
		}
	}
	Ok(())
}

/// Parse the `openapi` subcommand's arguments.
///
/// Accepted forms:
/// - `openapi` → stdout
/// - `openapi --out -` → stdout
/// - `openapi --out <path>` → write to file
fn parse_args(mut arguments: Vec<String>) -> Result<Option<String>, crate::cli::UsageError> {
	// `arguments[0]` is the `openapi` word the parser already matched.
	if arguments.len() <= 1 {
		return Ok(None);
	}
	let first = arguments[1].clone();
	match first.as_str() {
		"--out" => {
			if arguments.len() <= 2 {
				return Err(crate::cli::usage("openapi: `--out` needs a path argument; use `-` for stdout"));
			}
			if arguments.len() > 3 {
				return Err(crate::cli::usage(&format!("openapi: `{}` is not an argument of `--out`; usage: weft-server openapi [--out PATH]", arguments[3])));
			}
			let path = arguments.remove(2);
			if std::path::Path::new(&path).is_dir() {
				return Err(crate::cli::usage(&format!("openapi: `{path}` is a directory; `--out` expects a file path or `-` for stdout")));
			}
			Ok(Some(path))
		}
		"--help" | "-h" => Err(crate::cli::usage("openapi: generate the OpenAPI 3.1 document from the route registry\n\nusage: weft-server openapi [--out PATH]\n\nWith no `--out`, or `--out -`, the document is written to stdout.\nWith `--out <path>`, it is written to that file.\n\nThe command exits 0 on success and 78 on a refusal.")),
		other if other.starts_with('-') => Err(crate::cli::usage(&format!("openapi: unrecognised option `{other}`; only `--out PATH` is accepted, with `-` for stdout"))),
		other => Err(crate::cli::usage(&format!("openapi: unrecognised argument `{other}`"))),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::routes::build_registry;

	/// The document generated from the registry the server actually serves.
	fn generated() -> Value {
		from_table(&build_registry().table())
	}

	/// Where the committed document lives, resolved from the crate directory. A path relative to
	/// the working directory only lines up when the test binary happens to be run from one place;
	/// `cargo test -p weft-server` runs it from the workspace root, and the file is above there.
	fn committed_spec_path() -> std::path::PathBuf {
		std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("weft-server is a crate inside the workspace, so it has a parent directory").join("compat/openapi/weftdb.json")
	}

	/// Setting this rewrites the committed document instead of comparing against it. The name is
	/// the one `weft-physical-type`'s golden harness uses, because this is the same arrangement:
	/// bytes produced by the code, committed, and compared on every run.
	const UPDATE_VAR: &str = "WEFT_UPDATE_GOLDENS";

	/// The drift gate: the committed spec is the document the routes generate.
	/// A generated file nothing compares against is a second source of truth,
	/// which is what this slice exists to prevent.
	///
	/// `info.version` is normalised on both sides before comparison — every path,
	/// method, scope, tier and schema is still compared. The version is asserted
	/// separately against the crate version in `info_block_has_service_name_and_version`.
	/// This avoids the gate tripping on every release bump; see weftdb#620.
	#[test]
	fn generated_spec_matches_committed_file() {
		let path = committed_spec_path();
		let document = generated();
		// The same serialiser the `openapi` subcommand uses, so the file the test compares against
		// and the file an operator writes out with the CLI cannot differ by formatting alone.
		let fresh = serde_json::to_vec_pretty(&document).expect("the generated document is serialisable");
		if std::env::var_os(UPDATE_VAR).is_some() {
			std::fs::write(&path, fresh).unwrap_or_else(|error| panic!("{}: could not rewrite the committed spec: {error}", path.display()));
			return;
		}
		let committed = std::fs::read(&path).unwrap_or_else(|error| {
			panic!(
				"no committed spec at {} ({error}).\n\tCreate it with `{UPDATE_VAR}=1 cargo test -p weft-server --lib openapi`, \
				 then review the file as a fixture before committing it.",
				path.display()
			)
		});
		let committed_value: Value = serde_json::from_slice(&committed).unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()));

		// Normalise `info.version` on both sides so a crate version bump does not
		// break the gate — route shape is what matters, not the version string. See #620.
		let mut compare_gen = document.clone();
		let mut compare_com = committed_value.clone();
		let version_sentinel = Value::String("version::excluded-from-drift-comparison".to_string());
		if let Some(info) = compare_gen.get_mut("info") {
			if let Some(obj) = info.as_object_mut() {
				obj.insert("version".to_string(), version_sentinel.clone());
			}
		}
		if let Some(info) = compare_com.get_mut("info") {
			if let Some(obj) = info.as_object_mut() {
				obj.insert("version".to_string(), version_sentinel);
			}
		}

		let generated_paths = document["paths"].as_object().map_or(0, Map::len);
		let committed_paths = committed_value["paths"].as_object().map_or(0, Map::len);
		assert!(
			compare_gen == compare_com,
			"the committed spec at {} is not what the routes generate: {generated_paths} paths generated, {committed_paths} committed.\n\t\
			 (`info.version` is excluded from this comparison — it is asserted against the crate version in \
			 `info_block_has_service_name_and_version`.)\n\t\
			 If a route changed on purpose, regenerate it with `{UPDATE_VAR}=1 cargo test -p weft-server --lib openapi` and review the diff.",
			path.display()
		);
	}

	/// Structural validation of the generated document: every path item carries
	/// an operation, every operation a 2xx and an error response, every `$ref`
	/// an existing target, and no two operations a shared `operationId`.
	#[test]
	fn generated_spec_passes_structural_validation() {
		let issues = validate(&generated());
		assert!(issues.is_empty(), "the generated OpenAPI document has structural issues:\n  {}", issues.join("\n  "));
	}

	/// Every status the error table can produce has a documented component
	/// response, so a client is never handed a status with no shape.
	#[test]
	fn every_error_status_has_a_component_response() {
		let document = generated();
		let components = document["components"].as_object().expect("the document has a components section");
		let responses = components.get("responses").and_then(Value::as_object).expect("components has a responses section");
		for status in ERROR_STATUSES {
			let key = format!("Error{status}");
			let entry = responses.get(&key).unwrap_or_else(|| panic!("`{key}` is missing from `components.responses`"));
			assert!(entry["description"].as_str().is_some_and(|text| !text.is_empty()), "`{key}` has no description");
		}
	}

	/// The committed file is a documented API, not a placeholder: an empty
	/// `paths` object would satisfy a byte-comparison drift gate the day the
	/// routes regenerate onto it, and would ship a spec that documents nothing.
	#[test]
	fn committed_spec_is_a_documented_api() {
		// A regeneration run is precisely the moment the file on disk is the stale one, so this
		// check has nothing to say until the writer has run.
		if std::env::var_os(UPDATE_VAR).is_some() {
			return;
		}
		let path = committed_spec_path();
		let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{} must exist: create it with `{UPDATE_VAR}=1 cargo test -p weft-server --lib openapi`: {error}", path.display()));
		let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()));
		let paths = parsed["paths"].as_object().unwrap_or_else(|| panic!("{} has no `paths` object", path.display()));
		assert!(!paths.is_empty(), "{} documents no paths: the committed spec is a placeholder", path.display());
	}

	/// The version is the one the document claims to be, not a bare "3.1".
	#[test]
	fn openapi_version_is_3_1() {
		let document = generated();
		assert_eq!(document["openapi"], "3.1.0");
	}

	/// The document names the service and the version that produced it.
	#[test]
	fn info_block_has_service_name_and_version() {
		let document = generated();
		let info = document["info"].as_object().expect("the document has an info block");
		assert_eq!(info["title"], crate::SERVICE);
		assert_eq!(info["version"], crate::VERSION, "the version in the OpenAPI document must match the crate version");
	}

	/// One path item per registered template, counted from the registry rather
	/// than hardcoded, so adding a route cannot pass against a stale number.
	#[test]
	fn one_path_item_per_registered_template() {
		let table = build_registry().table();
		let mut templates: Vec<&str> = table.entries().iter().map(|entry| entry.path).collect();
		templates.sort_unstable();
		templates.dedup();
		assert!(!templates.is_empty(), "the registry registered no routes at all");
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		assert_eq!(paths.len(), templates.len(), "one path item per registered template");
	}

	/// No two operations share an `operationId`: it is the client's method name.
	#[test]
	fn no_duplicate_operation_ids() {
		let duplicates: Vec<String> = validate(&generated()).into_iter().filter(|issue| issue.contains("duplicate `operationId`")).collect();
		assert!(duplicates.is_empty(), "duplicate operationIds: {duplicates:?}");
	}

	/// Every operation carries the summary and id a generated client needs.
	#[test]
	fn every_operation_has_a_summary_and_an_id() {
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		for (path, item) in paths {
			let methods = item.as_object().unwrap_or_else(|| panic!("`{path}` is not an object"));
			for (method, operation) in methods {
				if method == "parameters" {
					continue;
				}
				assert!(!operation["summary"].as_str().unwrap_or_default().is_empty(), "operation `{path}`/{method} has no summary");
				assert!(!operation["operationId"].as_str().unwrap_or_default().is_empty(), "operation `{path}`/{method} has no operationId");
			}
		}
	}

	/// FRE-9b: every registered operation has an annotation. A route added
	/// without one fails here instead of shipping an undescribed operation.
	#[test]
	fn every_operation_is_annotated() {
		let table = build_registry().table();
		let missing: Vec<&str> = table.entries().iter().map(|entry| entry.spec.operation_id).filter(|id| annotation_for(id).is_none()).collect();
		assert!(missing.is_empty(), "operations without an FRE-9b annotation: {missing:?}; add them to annotation_for");
	}

	/// FRE-9b: every POST documents its request body, except the query-only
	/// and empty POSTs named here — the list is pinned from the handlers'
	/// signatures, so a POST that gains a body without documenting it fails
	/// here, and a POST listed here that gains a body fails here too.
	#[test]
	fn every_post_documents_its_body() {
		let no_body_posts: &[&str] = &["storage_downsample_post", "storage_downsample_csv_post", "storage_downsample_arrow_post", "storage_downsample_parquet_post", "storage_reconcile_store", "storage_reconcile_aspect", "storage_squash_aspect", "storage_compact_aspect", "storage_fsck", "storage_quarantine_purge_expired", "storage_quarantine_release", "storage_quarantine_purge", "storage_backup", "storage_restore_drill", "storage_restore_into"];
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		for (path, item) in paths {
			let methods = item.as_object().unwrap_or_else(|| panic!("`{path}` is not an object"));
			let Some(operation) = methods.get("post") else {
				continue;
			};
			let op_id = operation["operationId"].as_str().unwrap_or("");
			let has_body = operation.get("requestBody").is_some();
			if no_body_posts.contains(&op_id) {
				assert!(!has_body, "POST `{op_id}` is a no-body operation but documents a requestBody");
			} else {
				assert!(has_body, "POST `{op_id}` takes a body but documents none");
			}
		}
		// The pin is complete: every named no-body POST exists in the registry.
		let table = build_registry().table();
		for op_id in no_body_posts {
			assert!(table.by_operation(op_id).is_some(), "pinned no-body POST `{op_id}` is not a registered operation; the pin is stale");
		}
	}

	/// FRE-9b: the `tag.*` selector family (TAG-10's named gap — FRE-9a left
	/// it undescribed) is documented on exactly the operations whose handler
	/// reads it: the `SelectorQuery` reads and the CSV/Parquet ingest routes.
	/// The set is pinned from the handlers' signatures, so an operation that
	/// gains or loses the selector without updating its annotation fails here
	/// — the document alone cannot catch a wrong `selector` flag, because the
	/// flag itself decides what the document carries. (The stored-interpolate
	/// GETs take `StrictQuery`, not `SelectorQuery`, and are not in this set.)
	#[test]
	fn selector_routes_document_tag_selectors() {
		let selector_ops: &[&str] = &["storage_time_range", "storage_time_range_csv", "storage_time_range_parquet", "storage_points_list", "storage_downsample_get", "storage_downsample_post", "storage_downsample_csv_get", "storage_downsample_csv_post", "storage_downsample_arrow_get", "storage_downsample_arrow_post", "storage_downsample_parquet_get", "storage_downsample_parquet_post", "storage_point_at", "storage_points_at_multi", "storage_value_range", "storage_value_range_csv", "storage_value_range_parquet", "storage_value_points", "storage_ingest_csv", "storage_ingest_parquet"];
		let table = build_registry().table();
		for op_id in selector_ops {
			assert!(table.by_operation(op_id).is_some(), "pinned selector operation `{op_id}` is not a registered operation; the pin is stale");
			assert!(annotation_for(op_id).is_some_and(|annotation| annotation.selector), "pinned selector operation `{op_id}` is not flagged `selector` in its annotation");
		}
		for entry in table.entries() {
			let op_id = entry.spec.operation_id;
			if !selector_ops.contains(&op_id) {
				assert!(!annotation_for(op_id).is_some_and(|annotation| annotation.selector), "operation `{op_id}` is flagged `selector` but is not in the pinned selector set; its handler does not read the `tag.*` family");
			}
		}
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		for (path, item) in paths {
			let methods = item.as_object().unwrap_or_else(|| panic!("`{path}` is not an object"));
			for (method, operation) in methods {
				if method == "parameters" {
					continue;
				}
				let op_id = operation["operationId"].as_str().unwrap_or("");
				let annotation = annotation_for(op_id).unwrap_or_else(|| panic!("operation `{op_id}` has no annotation"));
				let params = operation.get("parameters").and_then(Value::as_array);
				let has_tag_star = params.is_some_and(|list| list.iter().any(|param| param["name"] == "tag.*"));
				let has_tag_match = params.is_some_and(|list| list.iter().any(|param| param["name"] == "tag_match"));
				if annotation.selector {
					assert!(has_tag_star, "operation `{op_id}` reads the `tag.*` family but documents no `tag.*` parameter");
					assert!(has_tag_match, "operation `{op_id}` reads the `tag.*` family but documents no `tag_match` parameter");
				} else {
					assert!(!has_tag_star, "operation `{op_id}` does not read the `tag.*` family but documents a `tag.*` parameter");
				}
			}
		}
	}

	/// FRE-9b: the probes answer anonymously and every other operation
	/// requires `BearerAuth` (SEC-6: the anonymous principal holds only the
	/// probe scope, and only while authentication is on).
	#[test]
	fn security_assignment_matches_the_scope_table() {
		let table = build_registry().table();
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		for entry in table.entries() {
			let methods = paths[entry.path].as_object().unwrap_or_else(|| panic!("path `{}` is not in the document", entry.path));
			let method_key = match entry.method {
				Method::GET => "get",
				Method::POST => "post",
				_ => panic!("unexpected method for {}", entry.spec.operation_id),
			};
			let operation = &methods[method_key];
			let security = operation["security"].as_array().unwrap_or_else(|| panic!("operation `{}` has no `security`", entry.spec.operation_id));
			if entry.spec.scope == Scope::Probe {
				assert!(security.is_empty(), "probe operation `{}` must be anonymous (`security: []`)", entry.spec.operation_id);
			} else {
				assert_eq!(security.len(), 1, "operation `{}` must require exactly `BearerAuth`", entry.spec.operation_id);
				assert!(security[0].get("BearerAuth").is_some(), "operation `{}` must require `BearerAuth`", entry.spec.operation_id);
			}
		}
	}

	/// FRE-9b: creation answers `201` — the declaration, and ingest except
	/// on an idempotent replay (`200` with the same body). Every other
	/// operation's success is `200` only, so the old shape (a bare `200` for
	/// every operation) cannot come back.
	#[test]
	fn creation_answers_201_and_the_rest_answer_200() {
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		for (path, item) in paths {
			let methods = item.as_object().unwrap_or_else(|| panic!("`{path}` is not an object"));
			for (method, operation) in methods {
				if method == "parameters" {
					continue;
				}
				let op_id = operation["operationId"].as_str().unwrap_or("");
				let responses = operation["responses"].as_object().unwrap_or_else(|| panic!("operation `{op_id}` has no responses"));
				let success: Vec<&str> = responses.keys().map(String::as_str).filter(|code| code.starts_with('2')).collect();
				match op_id {
					"storage_aspect_declare" => assert_eq!(success, ["201"], "declare must answer `201` only, got {success:?}"),
					"storage_ingest_points" | "storage_ingest_ilp" | "storage_ingest_csv" | "storage_ingest_parquet" => {
						assert_eq!(success, ["200", "201"], "ingest `{op_id}` must answer `201` (fresh seal) and `200` (idempotent replay), got {success:?}");
					}
					_ => assert_eq!(success, ["200"], "operation `{op_id}` must answer `200` only, got {success:?}"),
				}
			}
		}
	}
}
