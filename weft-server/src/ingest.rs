//! Getting points into the store: the JSON, CSV, line-protocol and Parquet
//! ingest endpoints, the ordering gate they share, and the seal that turns a
//! batch into one `.weftseg` frame.
//!
//! Every ingest path is the same three steps: parse the body into timestamps and
//! values, check the ordering the aspect's contract asks for, then commit the
//! batch through the store's one idempotent entry point,
//! [`weftdb::SegmentStore::ingest`] (D-S13), as one or more segments. That is
//! the only commit path the HTTP surface has (D-S14, crash-consistency §8), so
//! an ingest cannot slip past the encoding/tolerance constraint (hard
//! constraint #4 — a value that cannot be represented under the declared
//! encoding within tolerance is rejected, never silently downcast), and every
//! format shares the ledger, the phases and the receipt.
//!
//! The commit runs in a detached task the handler awaits (crash-consistency
//! §5.2, SEAL-1): a client disconnect cannot cancel a batch mid-commit, and a
//! panic inside the task comes back as a `JoinError`/caught panic the handler
//! maps by [`weftdb::IngestPhase`] (plan D21), exactly as ROB-3's catch layer
//! maps a panic that unwinds in place. The caller's idempotency key — the
//! `Idempotency-Key` header (visible ASCII, at most 128 bytes), or the JSON
//! body's `batch_id` — is scoped per aspect and shared by all principals
//! (plan C10); a request with no key commits under a fresh ephemeral key, so
//! its retry is the documented at-least-once of a keyless retry, never a
//! silent replay of an unrelated batch.
//!
//! The Parquet ingest lives here rather than in [`crate::storage`] so that every
//! way a batch can arrive shares one ordering gate and one commit.

use std::sync::{
	atomic::{AtomicU64, Ordering}, Arc
};

use axum::{
	body::Bytes, extract::{Path, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json
};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;
use weft_line_protocol::TimestampPrecision;
use weft_physical_type::{first_order_violation, TimeUnit, PAGED_SEGMENT_FORMAT_VERSION, SEGMENT_FORMAT_VERSION};

use crate::{
	error::{self, ApiError, StrictBody, StrictJson, StrictQuery, StrictText, TaggedQuery}, panic_catch, state::AppState
};

/// Environment variable naming the one body cap for the ingest routes
/// (plan C11, crash-consistency §8): a positive whole number of bytes,
/// read once at startup.
///
/// Every other route keeps its own limits — this
/// cap applies to the four ingest routes only, as a
/// [`DefaultBodyLimit`](axum::extract::DefaultBodyLimit) the registry
/// installs at registration.
pub const MAX_INGEST_BYTES_ENV: &str = "WEFT_MAX_INGEST_BYTES";

/// Environment variable naming the ingest routes' in-flight byte budget
/// (crash-consistency §8): the byte-weighted admission the registry's
/// gate enforces inside itself (plan C10).
///
/// A request whose body would
/// push the in-flight total past the budget is refused
/// `503 overloaded` with `Retry-After`, instead of queueing without
/// bound behind the ingests already running.
pub const INGEST_MAX_INFLIGHT_BYTES_ENV: &str = "WEFT_INGEST_MAX_INFLIGHT_BYTES";

/// Environment variable naming the row cap for the four ingest routes (ROB-7, plan C11's row limit).
///
/// A positive whole number of rows, counted on the decoded columns
/// — the rows the seal would write, whichever format carried them.
///
/// A body over it is
/// refused `413 too_many_rows` with the limit.
pub const MAX_INGEST_ROWS_ENV: &str = "WEFT_MAX_INGEST_ROWS";

/// Environment variable naming the decoded-byte cap for the four ingest routes
/// (ROB-7, plan C11's decoded-byte limit): a positive whole number of bytes of
/// *decoded* columns, not of body.
///
/// `WEFT_MAX_INGEST_BYTES` bounds what arrives on the
/// socket, which says nothing about what a compressed Parquet body expands to.
pub const MAX_INGEST_DECODED_BYTES_ENV: &str = "WEFT_MAX_INGEST_DECODED_BYTES";

/// The default body cap when `WEFT_MAX_INGEST_BYTES` is unset:
/// 256 MiB (crash-consistency §8).
pub const DEFAULT_MAX_INGEST_BYTES: usize = 256 * 1024 * 1024;

/// The default row cap when `WEFT_MAX_INGEST_ROWS` is unset: 10 million rows.
///
/// A batch
/// that size is a ~240 MB seal — the same order as the 256 MiB body cap, so the row
/// ceiling sits where the body ceiling already stands, and it bites on a Parquet body
/// that the body cap lets through.
pub const DEFAULT_MAX_INGEST_ROWS: usize = 10_000_000;

/// The default decoded-byte cap when `WEFT_MAX_INGEST_DECODED_BYTES` is unset.
///
/// 1 GiB,
/// four times the default body cap, so a compressed body gets room to expand without
/// the decode being able to grow past what a node's memory is sized for.
///
pub const DEFAULT_MAX_INGEST_DECODED_BYTES: u64 = 1024 * 1024 * 1024;

/// The ingest wiring's two limits (D-S14), read once at startup and
/// carried on the router state, so the registry the binary serves and a
/// test's registry can carry different caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestConfig {
	body_bytes: usize,
	inflight_bytes: u64,
	rows: usize,
	decoded_bytes: u64,
}

impl IngestConfig {
	/// The defaults: a 256 MiB body cap, and an in-flight budget of the
	/// same 256 MiB — one max-size body in flight by default, or any
	/// number of smaller ones whose bodies sum under it.
	pub const DEFAULT: Self = Self { body_bytes: DEFAULT_MAX_INGEST_BYTES, inflight_bytes: DEFAULT_MAX_INGEST_BYTES as u64, rows: DEFAULT_MAX_INGEST_ROWS, decoded_bytes: DEFAULT_MAX_INGEST_DECODED_BYTES };

	/// A configuration with an explicit body cap and the default
	/// in-flight budget (tests, embedders).
	#[must_use]
	pub const fn with_max_body_bytes(max_body_bytes: usize) -> Self {
		Self { body_bytes: max_body_bytes, ..Self::DEFAULT }
	}

	/// A configuration with an explicit in-flight byte budget and the
	/// default body cap (tests, embedders).
	#[must_use]
	pub const fn with_max_inflight_bytes(max_inflight_bytes: u64) -> Self {
		Self { inflight_bytes: max_inflight_bytes, ..Self::DEFAULT }
	}

	/// A configuration with an explicit row cap and everything else at its
	/// default (tests, embedders).
	#[must_use]
	pub const fn with_max_rows(max_rows: usize) -> Self {
		Self { rows: max_rows, ..Self::DEFAULT }
	}

	/// A configuration with an explicit decoded-byte cap and everything else at its
	/// default (tests, embedders).
	#[must_use]
	pub const fn with_max_decoded_bytes(max_decoded_bytes: u64) -> Self {
		Self { decoded_bytes: max_decoded_bytes, ..Self::DEFAULT }
	}

	/// The limits the two `WEFT_*` values describe (`None` for an unset
	/// variable, which leaves that limit at its default).
	///
	/// # Errors
	///
	/// A message naming the variable when a value is not a positive
	/// whole number, so a malformed setting fails startup instead of
	/// silently falling back to a default the operator did not ask for.
	pub fn from_env_values(max_body_bytes: Option<&str>, max_inflight_bytes: Option<&str>, max_rows: Option<&str>, max_decoded_bytes: Option<&str>) -> Result<Self, String> {
		let mut config = Self::DEFAULT;
		if let Some(raw) = max_body_bytes {
			config.body_bytes = raw.parse().map_err(|e| format!("{MAX_INGEST_BYTES_ENV}={raw:?} is not a positive integer number of bytes: {e}"))?;
			if config.body_bytes == 0 {
				return Err(format!("{MAX_INGEST_BYTES_ENV}={raw:?} is not a positive integer number of bytes: zero would refuse every ingest body"));
			}
		}
		if let Some(raw) = max_inflight_bytes {
			config.inflight_bytes = raw.parse().map_err(|e| format!("{INGEST_MAX_INFLIGHT_BYTES_ENV}={raw:?} is not a positive integer number of bytes: {e}"))?;
			if config.inflight_bytes == 0 {
				return Err(format!("{INGEST_MAX_INFLIGHT_BYTES_ENV}={raw:?} is not a positive integer number of bytes: zero would refuse every ingest"));
			}
		}
		if let Some(raw) = max_rows {
			config.rows = raw.parse().map_err(|e| format!("{MAX_INGEST_ROWS_ENV}={raw:?} is not a positive whole number of rows: {e}"))?;
			if config.rows == 0 {
				return Err(format!("{MAX_INGEST_ROWS_ENV}={raw:?} is not a positive whole number of rows: zero would refuse every ingest body"));
			}
		}
		if let Some(raw) = max_decoded_bytes {
			config.decoded_bytes = raw.parse().map_err(|e| format!("{MAX_INGEST_DECODED_BYTES_ENV}={raw:?} is not a positive integer number of bytes: {e}"))?;
			if config.decoded_bytes == 0 {
				return Err(format!("{MAX_INGEST_DECODED_BYTES_ENV}={raw:?} is not a positive integer number of bytes: zero would refuse every ingest body"));
			}
		}
		Ok(config)
	}

	/// The limits the process environment describes; see
	/// [`from_env_values`](Self::from_env_values).
	///
	/// # Errors
	///
	/// A set variable whose value is not a positive whole number.
	pub fn from_env() -> Result<Self, String> {
		Self::from_env_values(std::env::var(MAX_INGEST_BYTES_ENV).ok().as_deref(), std::env::var(INGEST_MAX_INFLIGHT_BYTES_ENV).ok().as_deref(), std::env::var(MAX_INGEST_ROWS_ENV).ok().as_deref(), std::env::var(MAX_INGEST_DECODED_BYTES_ENV).ok().as_deref())
	}

	/// The one body cap for the ingest routes, in bytes.
	#[must_use]
	pub const fn max_body_bytes(self) -> usize {
		self.body_bytes
	}

	/// The ingest routes' in-flight byte budget, in bytes.
	#[must_use]
	pub const fn max_inflight_bytes(self) -> u64 {
		self.inflight_bytes
	}

	/// The row cap on one decoded ingest body (ROB-7).
	#[must_use]
	pub const fn max_rows(self) -> usize {
		self.rows
	}

	/// The decoded-byte cap on one ingest body (ROB-7).
	#[must_use]
	pub const fn max_decoded_bytes(self) -> u64 {
		self.decoded_bytes
	}
}

/// Refuse a decoded body that is too big to seal (ROB-7, plan C11's two limits).
///
/// Both ceilings are measured here, on the columns the format's parser produced,
/// rather than on the body: a body cap cannot bound what a compressed Parquet file
/// expands to, and a row count cannot bound what those rows weigh when the decimals
/// are wide. Every format reaches the same two numbers, so a client that sizes a
/// batch for one format is sized for all four.
fn check_decoded_size(timestamps: &[i64], values: &[Option<BigDecimal>], limits: IngestConfig) -> Result<(), ApiError> {
	if timestamps.len() > limits.max_rows() {
		return Err(error::too_many_ingest_rows(timestamps.len(), limits.max_rows()));
	}
	let decoded = decoded_column_bytes(timestamps, values);
	if decoded > limits.max_decoded_bytes() {
		return Err(error::decoded_too_large(decoded, limits.max_decoded_bytes()));
	}
	Ok(())
}

/// The decoded columns' byte cost (ROB-7): a slot per row in each column, plus the
/// mantissa bytes each decimal actually carries.
///
/// An estimate, and deliberately generous about the fixed part: what it must not do is
/// read the cap as satisfied because the *body* was small. The mantissa term is what
/// makes a wide decimal cost more than a narrow one, which is the difference between
/// this and a row count.
fn decoded_column_bytes(timestamps: &[i64], values: &[Option<BigDecimal>]) -> u64 {
	let slots = (timestamps.len() + values.len()) as u64;
	let mantissas: u64 = values.iter().flatten().map(|value| value.as_bigint_and_exponent().0.bits() / 8 + 1).sum();
	slots * 16 + mantissas
}

impl Default for IngestConfig {
	fn default() -> Self {
		Self::DEFAULT
	}
}

/// One point in an ingest batch: an integer epoch timestamp (in the aspect's
/// declared [`TimeUnit`]) and its value as lossless decimal text, or `null` for an
/// absent/null row.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
	/// The caller's idempotency key, as the body's own spelling of the
	/// `Idempotency-Key` header (crash-consistency §8: the key comes from
	/// the header *or from `batch_id`*). The header wins when both are
	/// sent. Same rule as the header: visible ASCII, at most 128 bytes.
	#[serde(default)]
	pub batch_id: Option<String>,
	/// The tags naming the one series the batch writes (TAG-11), as a JSON object of
	/// string values. Absent, `null` or `{}` is the untagged series 0. A key given twice
	/// is refused `400 invalid_tag` rather than letting the last one win, as are a
	/// reserved `__` key and anything else the tag grammar refuses.
	#[serde(default)]
	pub tags: Option<JsonTags>,
}

/// The `tags` object of a JSON ingest body, kept as the pairs it was written as: a
/// plain map would silently keep the last of two equal keys, so the pairs go to
/// [`TagSet::from_pairs`](weftdb::TagSet::from_pairs), which refuses the repeat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsonTags(pub Vec<(String, String)>);

impl<'de> Deserialize<'de> for JsonTags {
	fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		/// Collects every entry of the object, repeats included.
		struct Pairs;
		impl<'de> serde::de::Visitor<'de> for Pairs {
			type Value = JsonTags;

			fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				formatter.write_str("an object of string tag values")
			}

			fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
				let mut pairs = Vec::with_capacity(map.size_hint().unwrap_or(0).min(weftdb::tags::MAX_TAGS + 1));
				while let Some((key, value)) = map.next_entry::<String, String>()? {
					pairs.push((key, value));
				}
				Ok(JsonTags(pairs))
			}
		}
		deserializer.deserialize_map(Pairs)
	}
}

impl JsonTags {
	/// The tag set these pairs name.
	///
	/// # Errors
	///
	/// `400 invalid_tag` for a repeated key, a reserved or malformed key, a bad value, or
	/// too many tags ([`error::from_tag_error`]).
	fn to_tag_set(&self) -> Result<weftdb::TagSet, ApiError> {
		weftdb::TagSet::from_pairs(self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))).map_err(|e| error::from_tag_error(&e))
	}
}

/// Response body for a successful ingest: the sealed segment's descriptor summary,
/// the inputs a later read prunes on.
#[derive(Debug, Clone, Serialize)]
pub struct IngestResponse {
	/// The aspect the batch was sealed into.
	pub aspect: String,
	/// The id assigned to the new segment (monotonic within the aspect).
	pub segment_id: u64,
	/// The `.weftseg` frame format version sealed (5 single-block, 6 paged): the
	/// body's layout. A tagged series' frame is that body inside a v7 envelope
	/// carrying its series key (TAG-7); the envelope is not reported here, and
	/// `byte_len` counts it.
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
	/// Every segment id the batch's commit assigned, in time order
	/// (crash-consistency §8). A batch under the encoded-frame target
	/// commits exactly one segment, so this is usually `[segment_id]`;
	/// `segment_id` stays as the first id for the clients that predate
	/// the list.
	pub segment_ids: Vec<u64>,
	/// The id of the series the batch wrote (TAG-11): `0` for the untagged series,
	/// otherwise the id the aspect's catalog holds for [`tags`](Self::tags), assigned
	/// by the commit that first wrote the series.
	pub series_id: u64,
	/// The tags of the series the batch wrote: `{}` for the untagged series 0.
	pub tags: std::collections::BTreeMap<String, String>,
	/// Whether this response is a replay: the request's idempotency key
	/// was already committed with the same batch, so nothing was written
	/// again and the recorded receipt is answered back (with status
	/// `200` and the `Idempotency-Replayed: true` header, §2(g)).
	pub replayed: bool,
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

/// The most bytes an idempotency key may carry (crash-consistency §8:
/// visible ASCII, at most 128 bytes).
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// Validate one caller-supplied idempotency key.
///
/// # Errors
///
/// [`ApiError::bad_request`] when the key is empty, over
/// [`MAX_IDEMPOTENCY_KEY_BYTES`], or not visible ASCII.
fn validate_idempotency_key(key: &str) -> Result<String, ApiError> {
	if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES || !key.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
		return Err(ApiError::bad_request(format!("an idempotency key must be visible ASCII of 1..={MAX_IDEMPOTENCY_KEY_BYTES} bytes, got {} byte(s)", key.len())));
	}
	Ok(key.to_string())
}

/// The idempotency key a request commits under: the `Idempotency-Key`
/// header when it carries one, else the body's own key (`batch_id`, JSON
/// format only), else a fresh ephemeral key. The header sits behind the
/// registry's gate and the auth layer (plan C10) — this function only
/// ever runs inside a handler both have already passed.
///
/// # Errors
///
/// [`ApiError::bad_request`] for a supplied key that is not visible
/// ASCII of at most 128 bytes (a non-ASCII header value cannot even be
/// read as text, which is the same refusal).
fn resolve_idempotency_key(headers: &HeaderMap, body_key: Option<&str>) -> Result<(String, bool), ApiError> {
	if let Some(value) = headers.get("idempotency-key") {
		let key = value.to_str().map_err(|_| ApiError::bad_request("the `Idempotency-Key` header is not visible ASCII".to_string()))?;
		return validate_idempotency_key(key).map(|key| (key, true));
	}
	if let Some(key) = body_key {
		return validate_idempotency_key(key).map(|key| (key, true));
	}
	Ok((ephemeral_key(), false))
}

/// A one-off ledger key for a request that supplied none: unique per
/// request (process id, a process-wide counter, and the wall clock), so
/// a keyless batch commits exactly once for this request and a keyless
/// retry — which cannot name the batch — stays at-least-once, as
/// documented (crash-consistency §2).
fn ephemeral_key() -> String {
	static COUNTER: AtomicU64 = AtomicU64::new(0);
	let count = COUNTER.fetch_add(1, Ordering::Relaxed);
	let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
	format!("http-ephemeral:{}:{count}:{nanos}", std::process::id())
}

/// Emit the `weft_audit` ingest event for a batch that just committed
/// (plan §5, D-S14: the event is emitted *after* COMMIT — a replay,
/// which commits nothing, emits none, and neither does a refusal).
/// The event rides the tracing pipeline under its own target, and —
/// since SEC-9 — it is also written into the fsynced audit file when
/// one is attached, with the same fields. The commit has already
/// landed when this runs, so a failed file write is logged, not
/// propagated (the audit log is poisoned by it, and the next audited
/// operation refuses fail-closed).
fn emit_ingest_audit(audit: Option<&Arc<crate::audit::AuditLog>>, aspect: &str, format: &str, keyed: bool, receipt: &weftdb::IngestReceipt) {
	let series_id = receipt_series_id(receipt);
	tracing::info!(target: "weft_audit", event = "ingest", aspect, format, keyed, rows = receipt.row_count, segments = receipt.segment_ids.len(), commit_epoch = receipt.commit_epoch, series_id, "ingest committed");
	if let Some(audit) = audit {
		audit.emit_event(
			"ingest",
			serde_json::json!({
				"aspect": aspect,
				"format": format,
				"keyed": keyed,
				"rows": receipt.row_count,
				"segments": receipt.segment_ids.len(),
				"commit_epoch": receipt.commit_epoch,
				"series_id": series_id,
			}),
		);
	}
}

/// The one series a committed batch wrote: an HTTP ingest is one series per request
/// (TAG-11), so its receipt names exactly one id, `0` for the untagged series.
fn receipt_series_id(receipt: &weftdb::IngestReceipt) -> u64 {
	receipt.series_ids.first().copied().unwrap_or(0)
}

/// Commit one parsed batch through the store's ingest entry point, in a
/// detached task, and build the response every format answers with.
///
/// The detached task is SEAL-1 (crash-consistency §5.2): the handler
/// awaits its `JoinHandle`, so dropping the handler future — a client
/// disconnect — detaches the task instead of cancelling the commit. A
/// panic inside the task is caught by [`weftdb::exec::catch`] inside an
/// ingest-phase scope and mapped by [`IngestPhase`](weftdb::IngestPhase)
/// (plan D21, the ROB-3 table); a panic that still escapes as a
/// `JoinError` is mapped the same way, with no phase to key on (the
/// pre-commit arm: nothing the task could have committed is claimed).
async fn commit_ingest(target: IngestTarget<'_>, format: &'static str, keyed: bool, batch: weftdb::IngestBatch, timestamps: &[i64], values: &[Option<BigDecimal>]) -> Result<Response, ApiError> {
	let IngestTarget { store, metrics, audit, aspect, headers: _, limits: _ } = target;
	// An undeclared aspect is a clean 404 (its encoding is unknown),
	// answered here as the handlers always answered it — the store's
	// entry point would also refuse, but only after its name check,
	// which would call a path-escaping name malformed (400) instead of
	// undeclared (404).
	if store.schema_for(aspect).await.map_err(|err| error::classify_read(&err))?.is_none() {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
	}
	let rows_per_page = batch.rows_per_page;
	let tags: std::collections::BTreeMap<String, String> = batch.groups.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
	let task_store = Arc::clone(store);
	let task_aspect = aspect.to_string();
	let handle = tokio::spawn(async move { weftdb::exec::ingest_phase_scope(weftdb::exec::catch(async move { task_store.ingest(&task_aspect, &batch).await })).await });
	let receipt = match handle.await {
		Ok(Ok(Ok(receipt))) => receipt,
		Ok(Ok(Err(error))) => return Err(error::classify_ingest(&error)),
		Ok(Err(caught)) => {
			tracing::error!(panic = %caught.message, phase = ?caught.ingest_phase, "ingest task panicked; mapped by IngestPhase (D-S14, D21)");
			return Err(panic_catch::map_ingest_panic(caught.ingest_phase, &caught.message));
		}
		Err(join) if join.is_panic() => {
			let message = weftdb::exec::panic_message(&*join.into_panic());
			tracing::error!(panic = %message, "ingest task panicked outside its phase catch; mapped by IngestPhase (D-S14, D21)");
			return Err(panic_catch::map_ingest_panic(None, &message));
		}
		Err(join) => {
			tracing::error!(error = %join, "ingest task ended without an outcome");
			return Err(ApiError::internal(format!("the ingest task ended without an outcome: {join}")));
		}
	};
	if !receipt.duplicate {
		emit_ingest_audit(audit, aspect, format, keyed, &receipt);
		metrics.record_ingest_seal(u64::try_from(receipt.row_count).unwrap_or(u64::MAX));
		if receipt.segment_ids.len() > 1 {
			metrics.record_ingest_extra_segments(u64::try_from(receipt.segment_ids.len() - 1).unwrap_or(u64::MAX));
		}
	}
	Ok(ingest_response(store, aspect, &receipt, tags, timestamps, values, rows_per_page).await)
}

/// Build an ingest response from a commit's receipt: the additive shape
/// of crash-consistency §8 — `segment_id` stays as the first id, and
/// `segment_ids`/`replayed` join it. The remaining descriptor fields
/// come from the first committed segment's index row when it can be
/// read back; a row that cannot be read (quarantined between commit and
/// answer, say) falls back to the values the request itself determines,
/// because the commit has already happened and its receipt is the
/// answer that matters.
async fn ingest_response(store: &Arc<weftdb::SegmentStore>, aspect: &str, receipt: &weftdb::IngestReceipt, tags: std::collections::BTreeMap<String, String>, timestamps: &[i64], values: &[Option<BigDecimal>], rows_per_page: Option<usize>) -> Response {
	let first_id = receipt.segment_ids.first().copied();
	let descriptor = match first_id {
		Some(id) => store.index().all(aspect).await.ok().and_then(|descriptors| descriptors.into_iter().find(|d| d.id == id)),
		None => None,
	};
	let requested_format_version = if rows_per_page.is_some() { PAGED_SEGMENT_FORMAT_VERSION } else { SEGMENT_FORMAT_VERSION };
	let response = IngestResponse { aspect: aspect.to_string(), segment_id: first_id.unwrap_or(0), format_version: descriptor.as_ref().map_or(requested_format_version, |d| d.format_version), row_count: receipt.row_count, null_count: descriptor.as_ref().filter(|_| receipt.segment_ids.len() == 1).map_or_else(|| values.iter().filter(|v| v.is_none()).count(), |d| d.null_count), byte_len: descriptor.as_ref().map_or(0, |d| d.byte_len), min_ts: receipt.min_ts, max_ts: receipt.max_ts, time_sorted: first_order_violation(timestamps).is_none(), segment_ids: receipt.segment_ids.clone(), series_id: receipt_series_id(receipt), tags, replayed: receipt.duplicate };
	let status = if receipt.duplicate { StatusCode::OK } else { StatusCode::CREATED };
	let mut response = (status, Json(response)).into_response();
	if receipt.duplicate {
		response.headers_mut().insert("Idempotency-Replayed", axum::http::HeaderValue::from_static("true"));
	}
	response
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
pub async fn ingest_points(State(state): State<AppState>, Path(aspect): Path<String>, headers: HeaderMap, StrictJson(request): StrictJson<IngestRequest>) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
	// SEC-9: the committed-ingest `weft_audit` event goes to the audit
	// file as well as the tracing pipeline (see `emit_ingest_audit`).
	let audit = state.audit_log().cloned();
	// Read the ingest ceilings before the state is dropped: the handler owns the
	// `AppState` by value and the inner call takes the limits, not the state.
	let limits = state.ingest_config();
	drop(state);
	// Time the commit path (not the store-unconfigured fast-fail above): this is the
	// ingest side of the north-star "predictable p95/p99 under ingest + query".
	let start = std::time::Instant::now();
	let result = ingest_points_inner(IngestTarget { store: &store, metrics: &metrics, audit: audit.as_ref(), aspect: &aspect, headers: &headers, limits }, request).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// Where an ingest request commits, and the request headers it carries: the pieces every
/// ingest format's body shares.
#[derive(Clone, Copy)]
struct IngestTarget<'a> {
	store: &'a Arc<weftdb::SegmentStore>,
	metrics: &'a crate::metrics::SharedMetrics,
	audit: Option<&'a Arc<crate::audit::AuditLog>>,
	aspect: &'a str,
	headers: &'a HeaderMap,
	/// The decoded-size ceilings (ROB-7) every format checks before sealing.
	limits: IngestConfig,
}

/// The body of [`ingest_points`], split out so the handler can record an error
/// metric for any failure path uniformly.
async fn ingest_points_inner(target: IngestTarget<'_>, request: IngestRequest) -> Result<Response, ApiError> {
	let IngestTarget { headers, limits, .. } = target;
	if request.points.is_empty() {
		return Err(ApiError::bad_request("no points to ingest (`points` is empty)".to_string()));
	}
	let (key, keyed) = resolve_idempotency_key(headers, request.batch_id.as_deref())?;
	let tags = request.tags.as_ref().map(JsonTags::to_tag_set).transpose()?.unwrap_or_default();
	let point_count = request.points.len();
	let mut timestamps = Vec::with_capacity(point_count);
	let mut values = Vec::with_capacity(point_count);
	// `storage.ingest.parse` child span (roadmap Phase 3 — the write-path analogue of
	// the compute-path parse span): the decimal-text → `BigDecimal` value lift of the
	// batch, timed apart from the commit so a `RUST_LOG` run attributes ingest value
	// parsing separately from the segment write.
	{
		let _parse = tracing::info_span!("storage.ingest.parse", point_count, format = "json").entered();
		for point in &request.points {
			timestamps.push(point.timestamp);
			match &point.value {
				None => values.push(None),
				Some(text) => {
					// A `NaN` or infinity is not "not a decimal" — it is a value with no
					// decimal form, and it gets the code the read side already uses for
					// the same condition (ROB-7).
					if weft_physical_type::is_non_finite_decimal_text(text) {
						return Err(error::non_finite_value(format!("value {text:?} (at timestamp {})", point.timestamp)));
					}
					let parsed: BigDecimal = weft_physical_type::parse_decimal(text).map_err(|e| error::decimal_error(&e, ApiError::bad_request(format!("value {text:?} (at timestamp {}) is not a decimal", point.timestamp))))?;
					values.push(Some(parsed));
				}
			}
		}
	}

	check_decoded_size(&timestamps, &values, limits)?;
	enforce_order_if_required(request.require_sorted, &timestamps)?;
	// `storage.ingest.seal` child span (roadmap Phase 3): the typed-column encode +
	// `.weftseg` write + control-plane index update — the write stage timed apart from
	// value parsing.
	let batch = weftdb::IngestBatch::new(key, timestamps.clone(), values.clone()).with_rows_per_page(request.rows_per_page).with_groups(tags);
	commit_ingest(target, "json", keyed, batch, &timestamps, &values).instrument(tracing::info_span!("storage.ingest.seal", point_count, format = "json")).await
}

/// Query parameters for the CSV ingest endpoint: an optional page height. The
/// series is named by `tag.<key>=value` parameters beside these ([`TaggedQuery`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
		} else if weft_physical_type::is_non_finite_decimal_text(value_text) {
			// ROB-7: a `NaN` cell is refused with the reason, not stored as a hole.
			return Err(error::non_finite_value(format!("CSV line {}: value {value_text:?}", index + 1)));
		} else {
			let parsed: BigDecimal = weft_physical_type::parse_decimal(value_text).map_err(|e| error::decimal_error(&e, ApiError::bad_request(format!("CSV line {}: value {value_text:?} is not a decimal", index + 1))))?;
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
pub async fn ingest_csv(State(state): State<AppState>, Path(aspect): Path<String>, headers: HeaderMap, TaggedQuery(params, tags): TaggedQuery<CsvIngestParams>, StrictText(body): StrictText) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
	// SEC-9: the committed-ingest `weft_audit` event goes to the audit
	// file as well as the tracing pipeline (see `emit_ingest_audit`).
	let audit = state.audit_log().cloned();
	// Read the ingest ceilings before the state is dropped: the handler owns the
	// `AppState` by value and the inner call takes the limits, not the state.
	let limits = state.ingest_config();
	drop(state);
	let start = std::time::Instant::now();
	let result = ingest_csv_inner(IngestTarget { store: &store, metrics: &metrics, audit: audit.as_ref(), aspect: &aspect, headers: &headers, limits }, tags, params.rows_per_page, params.require_sorted, &body).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// The body of [`ingest_csv`], split out so the handler records an error metric for
/// any failure path uniformly. Shares the commit path and
/// `error::classify_ingest` with the JSON ingest path, so a CSV-committed batch
/// is byte-identical to a JSON-committed one.
async fn ingest_csv_inner(target: IngestTarget<'_>, tags: weftdb::TagSet, rows_per_page: Option<usize>, require_sorted: bool, body: &str) -> Result<Response, ApiError> {
	let IngestTarget { headers, limits, .. } = target;
	let (key, keyed) = resolve_idempotency_key(headers, None)?;
	// `storage.ingest.parse` decode span (roadmap Phase 3): the CSV text → typed
	// `(timestamp, value)` columns, timed apart from the commit.
	let ParsedCsv { timestamps, values, any_null } = tracing::info_span!("storage.ingest.parse", format = "csv").in_scope(|| parse_csv_points(body))?;
	check_decoded_size(&timestamps, &values, limits)?;
	enforce_order_if_required(require_sorted, &timestamps)?;
	let point_count = timestamps.len();
	let batch = weftdb::IngestBatch::new(key, timestamps.clone(), values.clone()).with_rows_per_page(rows_per_page).with_groups(tags);
	commit_ingest(target, "csv", keyed, batch, &timestamps, &values).instrument(tracing::info_span!("storage.ingest.seal", point_count, any_null, format = "csv")).await
}

/// Query parameters for the ILP ingest endpoint: which field to seal, the wire
/// timestamp precision, and an optional page height.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IlpIngestParams {
	/// The line-protocol field key whose numeric values are sealed (one aspect ==
	/// one field).
	pub field: String,
	/// The line-protocol measurement whose records are sealed (TAG-11, required).
	/// Records of any other measurement are skipped; the records of this one that
	/// carry `field` must share one tag set, which names the series the batch writes.
	pub measurement: String,
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

/// Project a line-protocol payload onto the one series an ILP ingest writes (TAG-11):
/// the records of `measurement` that carry `field` with a timestamp, sorted by
/// timestamp, and the tag set they share.
///
/// Records of another measurement, and records without the field or a timestamp, are
/// skipped (an ILP file may interleave many). Before TAG-11 the tags of the contributing
/// records were checked for agreement and then dropped, so a tagged payload was written
/// to the untagged series; now they name the series.
///
/// # Errors
///
/// A malformed payload ([`error::from_line_protocol_error`]), `400 ambiguous_series`
/// when the contributing records carry two different tag sets (TAG-0: one series per
/// request), and `400 invalid_tag` when their tags break the tag grammar (a reserved
/// `__` key, a repeated key, too many tags, ...).
fn project_ilp_series(body: &str, measurement: &str, field: &str, precision: TimestampPrecision) -> Result<(Vec<splimes::Point>, weftdb::TagSet), ApiError> {
	let records = weft_line_protocol::parse(body).map_err(|err| error::from_line_protocol_error(&err))?;
	let mut points = Vec::new();
	let mut series: Option<(weft_line_protocol::SeriesKey, String)> = None;
	for (index, record) in records.iter().enumerate().filter(|(_, record)| record.measurement == measurement) {
		// ROB-7: `to_point` has no decimal for NaN or infinity and would drop the
		// record, storing a gap the sender never meant; refuse it instead.
		if record.field_is_non_finite(field) {
			return Err(error::non_finite_value(format!("record {} field `{field}`", index + 1)));
		}
		let Some(point) = record.to_point(field, precision) else {
			continue;
		};
		let key = record.series_key();
		match &series {
			None => series = Some((key, record.describe_series())),
			Some((first, first_described)) if *first != key => {
				return Err(ApiError::new(error::Code::AmbiguousSeries, format!("field `{field}` of measurement `{measurement}` carries points from more than one series: `{first_described}` and `{}`; one request writes one series — split the payload by series", record.describe_series())));
			}
			Some(_) => {}
		}
		points.push(point);
	}
	points.sort_by_key(|p| p.timestamp);
	let tags = match series {
		Some(((_, tags), _)) => weftdb::TagSet::from_pairs(tags).map_err(|e| error::from_tag_error(&e))?,
		None => weftdb::TagSet::new(),
	};
	Ok((points, tags))
}

/// Handle `POST /api/v1/storage/{aspect}/ilp`.
///
/// Parses an InfluxDB-Line-Protocol payload (the wire format TSBS / `InfluxDB` /
/// `QuestDB` speak) and seals the chosen field's values into `aspect`'s declared
/// schema.
///
/// The payload is the request body (`text/plain`); `measurement` (required since
/// TAG-11), `field`, `precision`, and `rows_per_page` are query parameters. The
/// records of `measurement` carrying `field` must share one tag set, which names the
/// series the batch writes (no tags: the untagged series 0). The parser is the shared, vendor-neutral
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
/// range, or values unrepresentable under the declared encoding/tolerance),
/// `ambiguous_series` (the field's points span more than one series; TAG-0),
/// `invalid_tag` (the series' tags break the tag grammar), `series_limit` (a new
/// series beyond `WEFT_MAX_SERIES_PER_ASPECT`), and [`ApiError::internal`] on a filesystem/control-plane failure.
pub async fn ingest_ilp(State(state): State<AppState>, Path(aspect): Path<String>, headers: HeaderMap, StrictQuery(params): StrictQuery<IlpIngestParams>, StrictText(body): StrictText) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
	// SEC-9: the committed-ingest `weft_audit` event goes to the audit
	// file as well as the tracing pipeline (see `emit_ingest_audit`).
	let audit = state.audit_log().cloned();
	// Read the ingest ceilings before the state is dropped: the handler owns the
	// `AppState` by value and the inner call takes the limits, not the state.
	let limits = state.ingest_config();
	drop(state);
	let start = std::time::Instant::now();
	let result = ingest_ilp_inner(IngestTarget { store: &store, metrics: &metrics, audit: audit.as_ref(), aspect: &aspect, headers: &headers, limits }, &params, &body).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// The body of [`ingest_ilp`], split out so the handler records an error metric for
/// any failure path uniformly.
async fn ingest_ilp_inner(target: IngestTarget<'_>, params: &IlpIngestParams, body: &str) -> Result<Response, ApiError> {
	let IngestTarget { store, aspect, headers, limits, .. } = target;
	let (key, keyed) = resolve_idempotency_key(headers, None)?;
	let precision = parse_ilp_precision(params.precision.as_deref()).map_err(ApiError::bad_request)?;
	let Some(schema) = store.schema_for(aspect).await.map_err(|err| error::classify_read(&err))? else {
		return Err(ApiError::aspect_not_found(format!("aspect `{aspect}` has no declared schema in the segment store")));
	};
	// `storage.ingest.parse` decode span (roadmap Phase 3): the line-protocol text →
	// typed points, timed apart from the timestamp normalization and the seal.
	let (points, tags) = tracing::info_span!("storage.ingest.parse", format = "ilp", field = %params.field, measurement = %params.measurement).in_scope(|| project_ilp_series(body, &params.measurement, &params.field, precision))?;
	if points.is_empty() {
		return Err(ApiError::bad_request(format!("no points carrying field `{}` with a timestamp in measurement `{}` of the payload", params.field, params.measurement)));
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
	// ILP fields are always present, so the batch's value column is dense
	// text widened to the nullable shape the one entry point takes.
	let nullable_values: Vec<Option<BigDecimal>> = values.into_iter().map(Some).collect();
	check_decoded_size(&timestamps, &nullable_values, limits)?;
	let batch = weftdb::IngestBatch::new(key, timestamps.clone(), nullable_values.clone()).with_rows_per_page(params.rows_per_page).with_groups(tags);
	commit_ingest(target, "ilp", keyed, batch, &timestamps, &nullable_values).instrument(tracing::info_span!("storage.ingest.seal", point_count, format = "ilp")).await
}

/// Query parameters for the Parquet ingest endpoint: an optional page height. The
/// series is named by `tag.<key>=value` parameters beside these ([`TaggedQuery`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
pub async fn ingest_parquet(State(state): State<AppState>, Path(aspect): Path<String>, headers: HeaderMap, TaggedQuery(params, tags): TaggedQuery<ParquetIngestParams>, StrictBody(body): StrictBody) -> Result<Response, ApiError> {
	let metrics = state.metrics().clone();
	metrics.record_ingest_request();
	let store = state.store().cloned().ok_or_else(|| {
		metrics.record_ingest_error();
		ApiError::unconfigured()
	})?;
	// SEC-9: the committed-ingest `weft_audit` event goes to the audit
	// file as well as the tracing pipeline (see `emit_ingest_audit`).
	let audit = state.audit_log().cloned();
	// Read the ingest ceilings before the state is dropped: the handler owns the
	// `AppState` by value and the inner call takes the limits, not the state.
	let limits = state.ingest_config();
	drop(state);
	let start = std::time::Instant::now();
	let result = ingest_parquet_inner(IngestTarget { store: &store, metrics: &metrics, audit: audit.as_ref(), aspect: &aspect, headers: &headers, limits }, tags, params.rows_per_page, params.require_sorted, &body).await;
	drop(store);
	if result.is_err() {
		metrics.record_ingest_error();
	}
	metrics.observe_ingest_latency(start.elapsed());
	result
}

/// The body of [`ingest_parquet`], split out so the handler records an error
/// metric for any failure path uniformly.
async fn ingest_parquet_inner(target: IngestTarget<'_>, tags: weftdb::TagSet, rows_per_page: Option<usize>, require_sorted: bool, body: &Bytes) -> Result<Response, ApiError> {
	let IngestTarget { aspect, headers, limits, .. } = target;
	if body.is_empty() {
		return Err(ApiError::bad_request("empty Parquet body".to_string()));
	}
	let (key, keyed) = resolve_idempotency_key(headers, None)?;
	// `storage.ingest.parquet` stage span (roadmap Phase 3 — the Arrow-decode-on-ingest
	// analogue of the JSON/CSV/ILP `storage.ingest.parse`+`seal` spans). The decode
	// lives in `weft-arrow-store` (so the heavy `arrow-*` tree stays off the lean
	// core) and the leaf stays tracing-free by design, so decode+commit ride one
	// span here, carrying the request's byte length and the sort-guard flag. The
	// decoded columns commit through the same entry point as the other three
	// formats (D-S14) — the decode is the only Parquet-specific step left.
	let commit = async {
		// The strict decode (ROB-7): a `NaN` or infinite value in the file is refused
		// with its row, not mapped to a null the sender never asked for.
		let (timestamps, values) = weft_arrow_store::decode_parquet_columns_strict(body, require_sorted).map_err(|err| error::classify_ingest(&err))?;
		// The two ceilings (ROB-7) land here rather than on the body: a Parquet file is
		// the one format whose body size says nothing about what it decodes to.
		check_decoded_size(&timestamps, &values, limits)?;
		let batch = weftdb::IngestBatch::new(key, timestamps.clone(), values.clone()).with_rows_per_page(rows_per_page).with_groups(tags);
		commit_ingest(target, "parquet", keyed, batch, &timestamps, &values).await
	};
	commit.instrument(tracing::info_span!("storage.ingest.parquet", %aspect, byte_len = body.len(), require_sorted, format = "parquet")).await
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
		let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));
		drop(store);
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
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=s&require_sorted=true", payload).await;
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
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=s", payload).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["aspect"], "temp");
		assert_eq!(body["row_count"], 3);
		assert_eq!(body["null_count"], 0);
		assert_eq!(body["min_ts"], 100);
		assert_eq!(body["max_ts"], 120);
		// TAG-11: the payload's tags name the series, no longer silently dropped.
		assert_eq!(body["tags"], serde_json::json!({ "loc": "a" }), "body: {body}");
		assert_eq!(body["series_id"], 1, "the first tagged series of the aspect: {body}");

		// Read it back through the JSON range endpoint, selecting the series.
		let router = router_with_declared_temp_reopened(&dir).await;
		let (_, read) = get_json(router, "/api/v1/storage/temp/points?start=100&end=120&tag.loc=a").await;
		assert_eq!(read["count"], 3);
		assert_eq!(read["points"][0]["value"], "1.5");
		// The untagged series holds nothing: the rows are not in series 0.
		let router = router_with_declared_temp_reopened(&dir).await;
		let (_, untagged) = get_json(router, "/api/v1/storage/temp/points?start=100&end=120").await;
		assert_eq!(untagged["count"], 0, "{untagged}");
	}

	#[tokio::test]
	async fn untagged_ilp_with_measurement_is_series_zero() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		// A record of another measurement is skipped, tags and all.
		let payload = "weather temp=1.5 100\nother,loc=b temp=9 105\nweather temp=2.5 110\n";
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=s", payload).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!((body["row_count"].clone(), body["series_id"].clone(), body["tags"].clone()), (serde_json::json!(2), serde_json::json!(0), serde_json::json!({})), "body: {body}");
		let router = router_with_declared_temp_reopened(&dir).await;
		let (_, read) = get_json(router, "/api/v1/storage/temp/points?start=0&end=200").await;
		assert_eq!(read["count"], 2, "{read}");
	}

	#[tokio::test]
	async fn ilp_without_measurement_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?field=temp&precision=s", "weather temp=1 100\n").await;
		assert_eq!((status, &body["code"]), (StatusCode::BAD_REQUEST, &serde_json::json!("bad_request")), "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("measurement"), "body: {body}");
	}

	#[tokio::test]
	async fn ilp_with_two_series_is_still_ambiguous_series() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		let payload = "weather,loc=a temp=1 100\nweather,loc=b temp=2 110\n";
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=s", payload).await;
		assert_eq!((status, &body["code"]), (StatusCode::BAD_REQUEST, &serde_json::json!("ambiguous_series")), "body: {body}");
	}

	#[tokio::test]
	async fn reserved_tag_key_is_invalid_tag() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=s", "weather,__dim=a temp=1 100\n").await;
		assert_eq!((status, &body["code"], &body["details"]["tag"]), (StatusCode::BAD_REQUEST, &serde_json::json!("invalid_tag"), &serde_json::json!("__dim")), "ilp: {body}");
		let router = router_with_declared_price_reopened(&dir).await;
		let (status, body) = post_json(router, "/api/v1/storage/temp/points", &serde_json::json!({ "tags": { "__dim": "a" }, "points": [{ "timestamp": 1, "value": "1" }] })).await;
		assert_eq!((status, &body["code"]), (StatusCode::BAD_REQUEST, &serde_json::json!("invalid_tag")), "json: {body}");
		let router = router_with_declared_price_reopened(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/temp/csv?tag.__dim=a", "1,1\n").await;
		assert_eq!((status, &body["code"]), (StatusCode::BAD_REQUEST, &serde_json::json!("invalid_tag")), "csv: {body}");
	}

	#[tokio::test]
	async fn json_duplicate_tag_key_is_invalid_tag() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		// Written as raw text: a JSON value cannot hold a repeated key.
		let raw = r#"{"tags":{"host":"a","host":"b"},"points":[{"timestamp":1,"value":"1"}]}"#;
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/points").header("content-type", "application/json").body(Body::from(raw)).unwrap()).await.unwrap();
		let status = response.status();
		let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
		assert_eq!((status, &body["code"], &body["details"]["tag"]), (StatusCode::BAD_REQUEST, &serde_json::json!("invalid_tag"), &serde_json::json!("host")), "body: {body}");
	}

	#[tokio::test]
	async fn csv_ingest_tag_params_name_the_series() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv?tag.host=a&rows_per_page=2", "100,1.5\n110,2.5\n").await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!((body["series_id"].clone(), body["tags"].clone()), (serde_json::json!(1), serde_json::json!({ "host": "a" })), "body: {body}");
		// An empty value, `tag_match`, and a repeated key are refused on a write.
		for (query, code) in [("tag.host=", "invalid_tag"), ("tag.host=a&tag_match=subset", "bad_request"), ("tag.host=a&tag.host=b", "invalid_tag")] {
			let router = router_with_declared_price_reopened(&dir).await;
			let (status, body) = post_text(router, &format!("/api/v1/storage/price/csv?{query}"), "1,1\n").await;
			assert_eq!((status, &body["code"]), (StatusCode::BAD_REQUEST, &serde_json::json!(code)), "{query}: {body}");
		}
	}

	#[tokio::test]
	async fn series_limit_is_400_series_limit() {
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open_with_options(dir.path(), weftdb::SegmentStoreOptions::default().with_max_series_per_aspect(1)).await.unwrap());
		store.declare("price", &AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds)).await.unwrap();
		let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));
		drop(store);
		let point = |host: &str| serde_json::json!({ "tags": { "host": host }, "points": [{ "timestamp": 1, "value": "1" }] });
		let (status, body) = post_json(router.clone(), "/api/v1/storage/price/points", &point("a")).await;
		assert_eq!(status, StatusCode::CREATED, "the first series fits: {body}");
		let (status, body) = post_json(router.clone(), "/api/v1/storage/price/points", &point("b")).await;
		assert_eq!((status, &body["code"]), (StatusCode::BAD_REQUEST, &serde_json::json!("series_limit")), "body: {body}");
		// A registered series stays writable, and series 0 does not count.
		let (status, body) = post_json(router.clone(), "/api/v1/storage/price/points", &point("a")).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &serde_json::json!({ "points": [{ "timestamp": 2, "value": "2" }] })).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
	}

	/// The end-to-end check that closes the tag core (TAG-5..TAG-11, #233): a tagged write
	/// over HTTP in each format is read back over HTTP by its selector, and only by it.
	#[tokio::test]
	async fn tagged_write_then_tagged_read_round_trips() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		// JSON into host=a (tags given out of key order), ILP into host=b, CSV into host=c,
		// Parquet into host=d, untagged into series 0.
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &serde_json::json!({ "tags": { "region": "eu", "host": "a" }, "points": [{ "timestamp": 100, "value": "1.5" }, { "timestamp": 110, "value": "2.5" }] })).await;
		assert_eq!((status, body["series_id"].clone(), body["tags"].clone()), (StatusCode::CREATED, serde_json::json!(1), serde_json::json!({ "host": "a", "region": "eu" })), "json: {body}");
		let router = router_with_declared_price_reopened(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/ilp?measurement=cpu&field=v&precision=s", "cpu,host=b v=7.5 100\n").await;
		assert_eq!((status, body["series_id"].clone()), (StatusCode::CREATED, serde_json::json!(2)), "ilp: {body}");
		let router = router_with_declared_price_reopened(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/price/csv?tag.host=c", "100,8.5\n").await;
		assert_eq!((status, body["series_id"].clone()), (StatusCode::CREATED, serde_json::json!(3)), "csv: {body}");
		let router = router_with_declared_price_reopened(&dir).await;
		let (status, body) = post_parquet(router, "/api/v1/storage/price/parquet?tag.host=d", price_parquet_body(&[100], &[Some(bd("9.5"))])).await;
		assert_eq!((status, body["series_id"].clone()), (StatusCode::CREATED, serde_json::json!(4)), "parquet: {body}");
		let router = router_with_declared_price_reopened(&dir).await;
		let (status, body) = post_json(router, "/api/v1/storage/price/points", &serde_json::json!({ "points": [{ "timestamp": 100, "value": "0.5" }] })).await;
		assert_eq!((status, body["series_id"].clone(), body["tags"].clone()), (StatusCode::CREATED, serde_json::json!(0), serde_json::json!({})), "untagged: {body}");

		for (query, values, tags) in [("&tag.host=a&tag.region=eu", vec!["1.5", "2.5"], serde_json::json!({ "host": "a", "region": "eu" })), ("&tag.host=b", vec!["7.5"], serde_json::json!({ "host": "b" })), ("&tag.host=c", vec!["8.5"], serde_json::json!({ "host": "c" })), ("&tag.host=d", vec!["9.5"], serde_json::json!({ "host": "d" })), ("", vec!["0.5"], serde_json::json!({}))] {
			let router = router_with_declared_price_reopened(&dir).await;
			let (status, read) = get_json(router, &format!("/api/v1/storage/price/points?start=0&end=200{query}")).await;
			assert_eq!(status, StatusCode::OK, "{query}: {read}");
			let got: Vec<&str> = read["points"].as_array().unwrap().iter().map(|p| p["value"].as_str().unwrap()).collect();
			assert_eq!((got, &read["tags"]), (values, &tags), "{query}: {read}");
		}
		// The selector `host=a` alone is exact, so it does not match `{host=a, region=eu}`.
		let router = router_with_declared_price_reopened(&dir).await;
		let (_, read) = get_json(router, "/api/v1/storage/price/points?start=0&end=200&tag.host=a").await;
		assert_eq!(read["count"], 0, "{read}");
		// Loose matching finds it.
		let router = router_with_declared_price_reopened(&dir).await;
		let (_, read) = get_json(router, "/api/v1/storage/price/points?start=0&end=200&tag.host=a&tag_match=subset").await;
		assert_eq!(read["count"], 2, "{read}");
	}

	#[tokio::test]
	async fn ilp_ingest_into_undeclared_aspect_is_not_found() {
		let dir = TempDir::new().unwrap();
		let router = router_with_empty_store(&dir).await;
		let (status, _body) = post_text(router, "/api/v1/storage/ghost/ilp?measurement=weather&field=temp&precision=s", "weather temp=1 100\n").await;
		assert_eq!(status, StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn ilp_ingest_unknown_precision_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=fortnights", "weather temp=1 100\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("unknown precision"));
	}

	#[tokio::test]
	async fn ilp_ingest_missing_field_is_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		// The payload carries `humidity`, but we ask for `temp`: no usable points.
		let (status, body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=s", "weather humidity=50 100\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("no points carrying field"));
	}

	#[tokio::test]
	async fn ilp_ingest_without_store_is_unavailable() {
		let router = app_with_state(AppState::new());
		let (status, _body) = post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp", "weather temp=1 100\n").await;
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
		let router = app_with_state(AppState::with_metrics(metrics.clone()).with_store(Arc::clone(&store)));
		drop(store);

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
		let router = app_with_state(AppState::with_metrics(metrics.clone()).with_store(Arc::clone(&store)));
		drop(store);
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

		let router = router_with_declared_empty_price(&dir).await;

		// An out-of-order Parquet batch (row 1: 90 < 100).

		let batch = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, &[100_i64, 90, 120], &[Some(bd("1")), Some(bd("2")), Some(bd("3"))]).expect("builds the batch");

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

		let router = app_with_state(AppState::new().with_store(Arc::clone(&store)));
		drop(store);

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
		let router = app_with_state(AppState::with_metrics(metrics.clone()).with_store(Arc::clone(&store)));
		drop(store);

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

	/// POST a JSON body with an `Idempotency-Key` header, returning the
	/// status, the parsed body, and the `Idempotency-Replayed` header.
	async fn post_keyed(router: axum::Router, uri: &str, key: &str, body: &serde_json::Value) -> (StatusCode, serde_json::Value, Option<String>) {
		let response = router.oneshot(Request::builder().method("POST").uri(uri).header("content-type", "application/json").header("idempotency-key", key).body(Body::from(serde_json::to_vec(body).unwrap())).unwrap()).await.unwrap();
		let status = response.status();
		let replayed = response.headers().get("idempotency-replayed").map(|value| value.to_str().unwrap().to_string());
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null), replayed)
	}

	fn price_batch() -> serde_json::Value {
		serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
		] })
	}

	#[tokio::test]
	async fn keyed_ingest_replays_the_original_receipt() {
		// D-S14 / crash §2(g): a keyed batch applies at most once; the
		// retry is answered with the original receipt as 200 +
		// `Idempotency-Replayed: true`, and no second copy lands.
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, body, replayed) = post_keyed(router.clone(), "/api/v1/storage/price/points", "batch-1", &price_batch()).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["replayed"], false);
		assert_eq!(body["segment_ids"].as_array().unwrap().len(), 1);
		assert_eq!(replayed, None, "a fresh commit carries no replay header");
		let segment_ids = body["segment_ids"].clone();

		let (status, body, replayed) = post_keyed(router, "/api/v1/storage/price/points", "batch-1", &price_batch()).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["replayed"], true);
		assert_eq!(body["segment_ids"], segment_ids, "the replay answers the original receipt");
		assert_eq!(replayed.as_deref(), Some("true"));

		let router = router_with_declared_price_reopened(&dir).await;
		let (_, read) = crate::test_support::get_json(router, "/api/v1/storage/price/points?start=100&end=120").await;
		assert_eq!(read["count"], 3, "the retried batch did not land twice");
	}

	#[tokio::test]
	async fn keyed_ingest_with_a_different_batch_conflicts() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, _, _) = post_keyed(router.clone(), "/api/v1/storage/price/points", "batch-1", &price_batch()).await;
		assert_eq!(status, StatusCode::CREATED);
		let other = serde_json::json!({ "points": [{ "timestamp": 100, "value": "9.5" }] });
		let (status, body, _) = post_keyed(router, "/api/v1/storage/price/points", "batch-1", &other).await;
		assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
		assert_eq!(body["code"], "idempotency_conflict", "body: {body}");
	}

	#[tokio::test]
	async fn body_batch_id_is_the_key_when_no_header_is_sent() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let mut batch = price_batch();
		batch["batch_id"] = serde_json::json!("from-body");
		let (status, body) = post_json(router.clone(), "/api/v1/storage/price/points", &batch).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		let (status, body, replayed) = post_keyed(router, "/api/v1/storage/price/points", "from-body", &price_batch()).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["replayed"], true);
		assert_eq!(replayed.as_deref(), Some("true"), "the header retry replays the batch_id commit: one key namespace");
	}

	#[tokio::test]
	async fn an_unusable_idempotency_key_is_a_bad_request() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let long = "k".repeat(129);
		let (status, body, _) = post_keyed(router, "/api/v1/storage/price/points", &long, &price_batch()).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
		assert!(body["error"].as_str().unwrap().contains("idempotency key"), "body: {body}");
	}

	#[tokio::test]
	async fn keyed_paged_ingest_seals_a_paged_frame_and_replays() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let batch = serde_json::json!({ "rows_per_page": 2, "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
			{ "timestamp": 130, "value": "4.5" },
		] });
		let (status, body, _) = post_keyed(router.clone(), "/api/v1/storage/price/points", "paged-1", &batch).await;
		assert_eq!(status, StatusCode::CREATED, "body: {body}");
		assert_eq!(body["format_version"], weft_physical_type::PAGED_SEGMENT_FORMAT_VERSION);
		let (status, body, _) = post_keyed(router, "/api/v1/storage/price/points", "paged-1", &batch).await;
		assert_eq!(status, StatusCode::OK, "body: {body}");
		assert_eq!(body["replayed"], true);
	}

	#[tokio::test]
	async fn a_body_over_the_configured_cap_is_payload_too_large() {
		// Crash §11's "a 10 MB CSV body is rejected" regression, at test
		// scale: the one body cap (C11) refuses the body before the
		// handler runs, as 413 with the limit.
		let dir = TempDir::new().unwrap();
		let store = store_with_declared_f64(&dir, "price").await;
		let config = super::IngestConfig::with_max_body_bytes(32);
		let router = app_with_state(AppState::new().with_store(store).with_ingest_config(config));
		let body = "timestamp,value\n100,1.5\n110,2.5\n120,3.5\n130,4.5\n";
		let (status, json) = crate::test_support::post_text(router, "/api/v1/storage/price/csv", body).await;
		assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "body: {json}");
		assert_eq!(json["code"], "payload_too_large", "body: {json}");
	}

	#[tokio::test]
	async fn a_json_nan_value_is_refused_as_not_finite() {
		// ROB-7: `"NaN"` parses as an `f64`, and a float with no decimal form used to
		// reach the seal as a value the operator never sent. `not_finite` is the code the
		// read side already uses for the same fact about a value.
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [ { "timestamp": 100, "value": "NaN" } ] });
		let (status, json) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {json}");
		assert_eq!(json["code"], "not_finite", "body: {json}");
	}

	#[tokio::test]
	async fn a_csv_nan_cell_is_refused_as_not_finite() {
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let (status, json) = crate::test_support::post_text(router, "/api/v1/storage/price/csv", "timestamp,value\n100,1.5\n110,-Infinity\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {json}");
		assert_eq!(json["code"], "not_finite", "body: {json}");
	}

	#[tokio::test]
	async fn an_infinity_value_is_refused_as_not_finite() {
		// The other spelling a float parser accepts: `"Infinity"` reaches `f64` as a
		// positive infinity through the JSON route, and is the same fact `"NaN"` is.
		//
		// This covers the JSON route's second spelling rather than a fifth ingest
		// format: the server's ingest formats are JSON points, ILP, CSV and Parquet —
		// there is no NDJSON route to POST to, so the earlier version of this test
		// asserted a 404.
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = serde_json::json!({ "points": [ { "timestamp": 110, "value": "Infinity" } ] });
		let (status, json) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {json}");
		assert_eq!(json["code"], "not_finite", "body: {json}");
	}

	#[tokio::test]
	async fn a_parquet_nan_cell_is_refused_as_not_finite() {
		// The issue's last open question: `null_count: 0` alongside a row the producer
		// never sent. A NaN Parquet cell is refused at the route, not sealed as a null.
		//
		// The bytes are a real Parquet file whose value column holds a raw IEEE-754 NaN,
		// built by `weft_arrow::write_raw_float_parquet` — the typed column builder in this
		// crate maps a non-finite float to a null, which is exactly the lenient behaviour
		// under test, so the file cannot be produced with it.
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_price(&dir).await;
		let body = {
			use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch};
			use arrow_schema::{DataType, Field, Schema};
			let schema = std::sync::Arc::new(Schema::new(vec![Field::new(weft_arrow::TIMESTAMP_COLUMN, DataType::Int64, false), Field::new(weft_arrow::VALUE_COLUMN, DataType::Float64, false)]));
			let batch = RecordBatch::try_new(schema, vec![std::sync::Arc::new(Int64Array::from(vec![100, 110])) as ArrayRef, std::sync::Arc::new(Float64Array::from(vec![1.5, f64::NAN])) as ArrayRef]).expect("builds a batch whose second value is a NaN");
			weft_arrow::write_parquet(std::slice::from_ref(&batch)).expect("writes parquet")
		};
		let (status, json) = post_parquet(router, "/api/v1/storage/price/parquet", body).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "a NaN Parquet cell is refused, not sealed as a null: {json}");
		assert_eq!(json["code"], "not_finite", "body: {json}");
	}

	#[tokio::test]
	async fn an_ilp_nan_field_is_refused_as_not_finite() {
		// The ILP hole was the quietest one: the line parsed, then dropped, so the
		// response counted one row fewer than the sender sent and stored a gap.
		let dir = TempDir::new().unwrap();
		let router = router_with_declared_temp(&dir).await;
		let (status, json) = crate::test_support::post_text(router, "/api/v1/storage/temp/ilp?measurement=weather&field=temp&precision=s", "weather temp=1 100\nweather temp=NaN 110\n").await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "body: {json}");
		assert_eq!(json["code"], "not_finite", "body: {json}");
	}

	#[tokio::test]
	async fn a_batch_over_the_row_cap_is_too_many_rows() {
		// ROB-7's row ceiling: the body cap cannot express "too many rows" for a
		// compressed Parquet body, so the decoded row count is checked before the seal.
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		store.declare("price", &AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds)).await.expect("declares");
		let router = app_with_state(AppState::new().with_store(std::sync::Arc::clone(&store)).with_ingest_config(super::IngestConfig::with_max_rows(2)));
		drop(store);
		let body = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
		] });
		let (status, json) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "body: {json}");
		assert_eq!(json["code"], "too_many_rows", "body: {json}");
		assert!(json["error"].as_str().unwrap().contains("WEFT_MAX_INGEST_ROWS"), "an operator ceiling names itself: {json}");
	}

	#[tokio::test]
	async fn a_decoded_column_set_over_the_byte_cap_is_payload_too_large() {
		// The decoded ceiling, at test scale: three seconds-resolution rows are 96 bytes
		// of columns plus their mantissas, so a 64-byte ceiling refuses them.
		let dir = TempDir::new().unwrap();
		let store = Arc::new(SegmentStore::open(dir.path()).await.expect("opens store"));
		store.declare("price", &AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds)).await.expect("declares");
		let router = app_with_state(AppState::new().with_store(std::sync::Arc::clone(&store)).with_ingest_config(super::IngestConfig::with_max_decoded_bytes(64)));
		drop(store);
		let body = serde_json::json!({ "points": [
			{ "timestamp": 100, "value": "1.5" },
			{ "timestamp": 110, "value": "2.5" },
			{ "timestamp": 120, "value": "3.5" },
		] });
		let (status, json) = post_json(router, "/api/v1/storage/price/points", &body).await;
		assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "body: {json}");
		assert_eq!(json["code"], "payload_too_large", "body: {json}");
		assert!(json["error"].as_str().unwrap().contains("WEFT_MAX_INGEST_DECODED_BYTES"), "the ceiling names itself: {json}");
	}

	#[test]
	fn the_parquet_route_decodes_under_the_ingest_rules() {
		// The Parquet half of ROB-7 is a choice of entry point, and the only honest
		// proof of a choice is the source: the lenient decoder must stay on the export
		// and re-seal paths, never on the write path (weft-arrow's no-panic lint is the
		// same shape of test, over the production half for the same reason).
		let source = include_str!("ingest.rs");
		let production = source.split("#[cfg(test)]").next().unwrap_or(source);
		assert!(production.contains("decode_parquet_columns_strict("), "the Parquet route must decode strict, or a NaN body stores a null");
		assert!(!production.contains("decode_parquet_columns("), "the lenient decoder has no business on a write path");
	}

	#[tokio::test]
	async fn admission_refuses_an_ingest_over_the_inflight_budget() {
		// D-S14, inside the gate (C10): a request whose declared body
		// alone exceeds the in-flight budget is refused 503 overloaded
		// with Retry-After, before any handler runs.
		let dir = TempDir::new().unwrap();
		let store = store_with_declared_f64(&dir, "price").await;
		let config = super::IngestConfig::with_max_inflight_bytes(8);
		let router = app_with_state(AppState::new().with_store(store).with_ingest_config(config));
		let payload = serde_json::to_vec(&price_batch()).unwrap();
		let response = router.oneshot(Request::builder().method("POST").uri("/api/v1/storage/price/points").header("content-type", "application/json").header("content-length", payload.len().to_string()).body(Body::from(payload)).unwrap()).await.unwrap();
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
		assert_eq!(response.headers().get("retry-after").unwrap(), "1");
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(json["code"], "overloaded", "body: {json}");
	}

	#[test]
	fn ingest_config_parses_and_refuses_zero() {
		let config = super::IngestConfig::from_env_values(Some("1024"), Some("2048"), Some("40"), Some("8192")).expect("valid");
		assert_eq!(config.max_body_bytes(), 1024);
		assert_eq!(config.max_inflight_bytes(), 2048);
		let defaults = super::IngestConfig::from_env_values(None, None, None, None).expect("defaults");
		assert_eq!(defaults.max_body_bytes(), super::DEFAULT_MAX_INGEST_BYTES);
		assert!(super::IngestConfig::from_env_values(Some("0"), None, None, None).is_err());
		assert!(super::IngestConfig::from_env_values(None, Some("many"), None, None).is_err());
		// ROB-7: the two new ceilings refuse a zero the same way, because a zero row
		// cap or decoded cap refuses every body.
		assert!(super::IngestConfig::from_env_values(None, None, Some("0"), None).is_err());
		assert!(super::IngestConfig::from_env_values(None, None, None, Some("0")).is_err());
		assert_eq!(super::IngestConfig::from_env_values(None, None, Some("40"), Some("8192")).expect("valid").max_rows(), 40);
	}
}
