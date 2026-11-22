//! Seal-backed legacy aspects (crash-consistency design §8, slice **S19**;
//! 1.0 plan §5 D-S19/D-S20 amendment and §6 W5 store lane).
//!
//! A legacy aspect is either **rows mode** (the default: measurements live in
//! its per-aspect `measurements.db`, as before this slice) or **seal-backed**
//! (`storage_mode = 'segments'`): its measurements are sealed into a
//! Storage-v2 [`SegmentStore`] rooted at `{data_dir}/{db}/segments_v2/`,
//! scoped `(db, 'legacy')` and keyed by the aspect's UUID, and every legacy
//! measurement reader for the aspect reads through that store. The mode is
//! chosen at aspect creation — explicitly, or for every new aspect while
//! `WEFT_LEGACY_DEFAULT_STORAGE=segments` — and is never switched afterwards:
//! there are no union reads. An aspect with a `compression_config` stays in
//! rows mode, because compression deletes ranges and the seal store has no
//! range delete (design §8, S19).
//!
//! **Writes.** `batch_capture_measurements` on a seal-backed aspect is one
//! [`SegmentStore::ingest`] call under a per-call token (`UUIDv4` when the
//! caller supplies none), so each logical write is one transaction —
//! all-or-nothing — and a retry of the same call with the same explicit
//! token replays the first commit's receipt instead of storing the rows
//! twice. The returned `TxIds` are synthetic: `UUIDv5` over the
//! idempotency key and the row index (the only production caller ignores
//! them). `capture_measurement` is a one-row ingest, followed by the
//! store's O(1) fragmentation gate.
//!
//! **Change log (D-S20).** Seal-backed ingest never touches the
//! per-timestamp unbatched queue rows mode feeds. Each seal-store commit
//! records one `segment_changes` row in its own index transaction — the
//! commit's epoch, time span and row count — so the batch consumer learns
//! about a commit from the change log
//! ([`SegmentStore::changes_since`](crate::SegmentStore::changes_since))
//! and acknowledges the rows it has batched
//! ([`SegmentStore::ack_changes`](crate::SegmentStore::ack_changes)).
//! Because the row commits with the segments, there is no write-ahead
//! entry for a commit that never landed and no landed commit without its
//! row: the S18 write-ahead/requeue dance the queue needed has no
//! counterpart here. Rows-mode aspects keep the queue unchanged.
//!
//! **Encoding.** On first use the adapter declares the aspect
//! [`PhysicalType::Decimal128`] at zero tolerance, in milliseconds (the
//! legacy timestamp unit). If a later batch holds a value Decimal128 would
//! lose, the declaration widens — through
//! [`widen_value_encoding`](weft_physical_type::widen_value_encoding), the
//! FRE-7b helper the plan names — to [`PhysicalType::BigDecimalText`] for
//! later frames. Frames self-describe, so a widened aspect keeps its earlier
//! frames readable and nothing is silently downcast.
//!
//! **Read-back identity.** Seal frames carry no per-row id and no dataset
//! id (the design's reader list for seal-backed aspects is raw, chunk,
//! range, count, boundary and earliest/latest — none of which frames key by
//! either), so a measurement read back from the store synthesises its id
//! deterministically as `UUIDv5` over `(aspect, timestamp, value, occurrence)`
//! and carries the nil dataset id. Two reads of the same stored row agree
//! on the id, which is what the boundary reader's de-duplication needs.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bigdecimal::BigDecimal;
use uuid::Uuid;
use sha2::{Digest, Sha256};
use weft_physical_type::{crc32, widen_value_encoding, AspectSchema, PhysicalType, TimeUnit};

use crate::{AspectId, DatasetId, IngestBatch, Measurement, MeasurementId, SegmentStore, TxId};

/// The environment variable naming the storage mode of a newly created legacy aspect.
///
/// Crash-consistency design §8, S19; #768 flipped the default. `rows` asks for
/// the per-aspect `measurements.db`, `segments` asks for the seal store, and
/// **unset is now seal-backed**.
///
/// A value that names neither word is rows mode, which is the safe side of the
/// ambiguity: a typo leaves the aspect in the shape every released Weft can read
/// instead of sealing it silently. Only the unset case changed —
/// [`StorageMode::parse`], which reads the mode an aspect already has, still
/// answers rows mode for `NULL` and for anything it does not recognise.
pub const LEGACY_DEFAULT_STORAGE_ENV: &str = "WEFT_LEGACY_DEFAULT_STORAGE";

/// The subject half of the seal store's scope: `(db, 'legacy')`.
pub const LEGACY_SCOPE_SUBJECT: &str = "legacy";

/// The directory, under a legacy database's directory, that holds its
/// seal-backed aspects' Storage-v2 root.
pub const SEGMENTS_ROOT_DIR: &str = "segments_v2";

/// The target segment size, in rows, of the fragmentation gate a one-row legacy ingest runs after
/// sealing (design §8, S19).
///
/// The gate itself is O(1) on the aspect's rollup, and a converged aspect is left alone.
pub const SQUASH_TARGET_ROWS: usize = 100_000;

/// How a legacy aspect stores its measurements, recorded on its
/// `aspects.storage_mode` row at creation and never changed afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum StorageMode {
	/// Measurements in the per-aspect `measurements.db`: the historical
	/// shape, the mode of every aspect a released Weft created, and still the
	/// mode a stored `NULL` parses to. It is no longer the mode new aspects are
	/// created in (#768) — see [`StorageMode::for_new_aspect`].
	#[default]
	Rows,
	/// Measurements sealed in the database's Storage-v2 store.
	Segments,
}

impl StorageMode {
	/// The value recorded in `aspects.storage_mode`.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Rows => "rows",
			Self::Segments => "segments",
		}
	}

	/// The mode a stored value names; anything else (including `NULL`, the
	/// value a pre-S19 row carries) is rows mode, the mode every existing
	/// aspect was created in.
	#[must_use]
	pub fn parse(value: Option<&str>) -> Self {
		match value {
			Some("segments") => Self::Segments,
			_ => Self::Rows,
		}
	}

	/// `true` for a seal-backed aspect.
	#[must_use]
	pub const fn is_seal_backed(self) -> bool {
		matches!(self, Self::Segments)
	}

	/// The mode a new aspect is created in (#768 flipped the unset case): it is
	/// seal-backed unless a compression config forces rows mode — compression
	/// deletes ranges, so it stays rows mode — or [`LEGACY_DEFAULT_STORAGE_ENV`]
	/// names rows mode. A value that names neither word answers through
	/// [`StorageMode::parse`], so a typo falls back to rows mode rather than
	/// sealing an aspect that an older server cannot read back.
	#[must_use]
	pub fn for_new_aspect(has_compression_config: bool, env_default: Option<&str>) -> Self {
		if has_compression_config {
			return Self::Rows;
		}
		env_default.map_or(Self::Segments, |value| Self::parse(Some(value)))
	}
}

/// The Storage-v2 root of the legacy database whose directory is `db_dir`.
#[must_use]
pub fn segments_root(db_dir: &Path) -> PathBuf {
	db_dir.join(SEGMENTS_ROOT_DIR)
}

/// The seal store's aspect key for a legacy aspect: its UUID, which is
/// unique across databases sharing nothing and stable across opens.
#[must_use]
pub fn aspect_key(aspect_id: &AspectId) -> String {
	aspect_id.as_uuid().to_string()
}

/// The canonical bytes of a batch's rows: each row as its timestamp in
/// milliseconds (little-endian) followed by its value's plain string.
/// Used by both [`payload_crc`] and [`payload_digest`] so the two digests
/// are derived from identical input.
#[must_use]
pub fn canonical_payload_bytes(rows: &[(i64, BigDecimal)]) -> Vec<u8> {
	let mut bytes = Vec::with_capacity(rows.len() * 24);
	for (timestamp, value) in rows {
		bytes.extend_from_slice(&timestamp.to_le_bytes());
		bytes.extend_from_slice(value.to_plain_string().as_bytes());
		bytes.push(0);
	}
	bytes
}

/// The CRC-32 of a batch's canonical rows. The ledger key no longer uses
/// this value because 32 bits collide at benchmark-relevant row counts
/// (~50 % odds by 77k distinct writes); see [`payload_digest`] for the
/// replacement.
#[must_use]
pub fn payload_crc(rows: &[(i64, BigDecimal)]) -> u32 {
	crc32(&canonical_payload_bytes(rows))
}

/// A 128-bit (SHA-256 prefix) digest over the canonical bytes of a batch's
/// rows, used in the derived idempotency key. Wider than [`payload_crc`] to
/// avoid silent collisions: the chance of an accidental collision across
/// distinct payloads is approximately 2⁻¹²⁸ per pair.
#[must_use]
pub fn payload_digest(rows: &[(i64, BigDecimal)]) -> [u8; 16] {
	let mut hasher = Sha256::new();
	hasher.update(canonical_payload_bytes(rows));
	let full: [u8; 32] = hasher.finalize().into();
	full[..16].try_into().expect("sha256 output is 32 bytes")
}

/// The derived idempotency key of one legacy ingest call (design §8, S19):
/// `legacy:{dataset_id}:{payload_digest:32 hex}:{n}`, `n` the row count.
/// The digest was widened from CRC-32 to a 128-bit SHA-256 prefix in
/// [#849] because CRC-32 collisions silently dropped distinct batches.
#[must_use]
pub fn idempotency_key(dataset_id: &DatasetId, rows: &[(i64, BigDecimal)]) -> String {
	let digest = payload_digest(rows);
	let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
	format!("legacy:{}:{hex}:{}", dataset_id.as_uuid(), rows.len())
}

/// The seal-store idempotency key of an identity-aware call: `legacy:{token}`.
///
/// The token is the caller's per-call identity — what a retry must reproduce —
/// and the payload is deliberately absent from the key. Two genuinely new calls
/// therefore get two keys even when their rows are identical, which the derived
/// [`idempotency_key`] cannot express: at that surface a retry and an identical
/// new submission are indistinguishable by construction (issue #803).
#[must_use]
pub fn idempotency_key_for_token(token: &str) -> String {
	format!("legacy:{token}")
}

/// A `UUIDv5` (RFC 4122 §4.3, SHA-1 name-based) over `name` in the URL namespace.
///
/// The workspace's `uuid` build does not enable its `v5` feature, and the synthetic ids of this module
/// need no other hash, so the digest is computed here.
#[must_use]
pub fn uuid_v5(name: &[u8]) -> Uuid {
	// The URL namespace, 6ba7b811-9dad-11d1-80b4-00c04fd430c8.
	const NAMESPACE_URL: [u8; 16] = [0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8];
	let mut sha1 = Sha1::new();
	sha1.update(&NAMESPACE_URL);
	sha1.update(name);
	let digest = sha1.finish();
	let mut bytes = [0u8; 16];
	bytes.copy_from_slice(&digest[..16]);
	bytes[6] = (bytes[6] & 0x0f) | 0x50; // version 5
	bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
	Uuid::from_bytes(bytes)
}

/// The synthetic `TxId` of row `index` of the ingest call named `key`:
/// `UUIDv5` over the key and the row index (design §8, S19).
#[must_use]
pub fn synthetic_tx_id(key: &str, index: usize) -> TxId {
	TxId::from_uuid(uuid_v5(format!("{key}:{index}").as_bytes()))
}

/// The deterministic id of a measurement read back from a seal frame:
/// `UUIDv5` over the aspect key, the row's timestamp and plain value, and
/// its occurrence among rows sharing that timestamp and value.
#[must_use]
pub fn synthetic_measurement_id(aspect_key: &str, timestamp_millis: i64, value: &BigDecimal, occurrence: usize) -> MeasurementId {
	MeasurementId::from_uuid(uuid_v5(format!("{aspect_key}:{timestamp_millis}:{}:{occurrence}", value.to_plain_string()).as_bytes()))
}

/// Open (creating if absent) the seal store of the legacy database `db_name`
/// whose directory is `db_dir`, under the `(db, 'legacy')` scope.
///
/// # Errors
///
/// As [`SegmentStore::open_scoped`].
pub async fn open_store(db_name: &str, db_dir: &Path) -> Result<SegmentStore> {
	SegmentStore::open_scoped(segments_root(db_dir), db_name, LEGACY_SCOPE_SUBJECT).await
}

/// Declare `key` in `store` on first use, and widen its declaration for
/// `values` when they need it (see the module documentation). Returns the
/// encoding the batch must be ingested under.
///
/// # Errors
///
/// A failed catalog read or declaration write.
pub async fn ensure_declared(store: &SegmentStore, key: &str, values: &[BigDecimal]) -> Result<PhysicalType> {
	let Some(schema) = store.schema_for(key).await? else {
		let widened = widen_value_encoding(PhysicalType::Decimal128, values);
		store.declare(key, &AspectSchema::new(widened, BigDecimal::from(0), TimeUnit::Millis)).await?;
		return Ok(widened);
	};
	let current = schema.value;
	let widened = widen_value_encoding(current, values);
	if widened != current {
		store.declare(key, &AspectSchema::new(widened, schema.value_tolerance.clone(), schema.timestamp_unit)).await?;
	}
	Ok(widened)
}

/// Ingest one legacy batch under a fully formed seal idempotency key.
///
/// `idem` is the key exactly as it goes to [`SegmentStore::ingest`] and as
/// `synthetic_tx_id` derives the receipt from; it is never re-wrapped here.
/// Both key styles share this body, so the fragmentation gate and the overlap
/// reconciliation of #813 run identically under the payload-derived
/// [`idempotency_key`] and the identity-aware [`idempotency_key_for_token`].
///
/// A gate error is logged at `tracing::debug!` and **never propagated**:
/// an ingest must not fail because the maintenance step did. The returned
/// `TxIds` are the same whether the gate ran or not.
///
/// # Errors
///
/// As [`SegmentStore::ingest`].
async fn ingest_batch_under_key(store: &SegmentStore, aspect_id: &AspectId, idem: &str, rows: &[(i64, BigDecimal)], squash_target_rows: usize) -> Result<Vec<TxId>> {
	let key = aspect_key(aspect_id);
	let values: Vec<BigDecimal> = rows.iter().map(|(_, value)| value.clone()).collect();
	ensure_declared(store, &key, &values).await?;
	let timestamps: Vec<i64> = rows.iter().map(|(timestamp, _)| *timestamp).collect();
	let receipt = store.ingest(&key, &IngestBatch::dense(idem.to_string(), timestamps, values)).await?;
	debug_assert_eq!(receipt.row_count, rows.len());
	if let Err(e) = store.squash_aspect_to_target_rows_if_fragmented(&key, squash_target_rows).await {
		tracing::debug!("Failed to run the fragmentation gate after ingesting a batch for aspect {aspect_id}; it is sealed: {e}");
	}
	// Guarantee at write time that the batch's timestamps do not overlap an existing
	// segment's extent for the same series without folding that cluster first.
	// `reconcile_overlaps` folds oldest → newest with newer-wins semantics, so after
	// this call the segment index's row count matches what a range read returns.
	// A no-op when nothing overlaps.
	if let Err(e) = store.reconcile_overlaps(&key).await {
		tracing::debug!("Failed to reconcile overlaps after ingesting a batch for aspect {aspect_id}; the count and the read may disagree: {e}");
	}
	Ok((0..rows.len()).map(|index| synthetic_tx_id(idem, index)).collect())
}

/// Ingest one legacy batch under the caller's per-call identity token.
///
/// The identity-aware entry point (issue #803): `idempotency_token` is **required**
/// so a new caller cannot silently opt out of replay protection, and the key is
/// `legacy:{token}` from [`idempotency_key_for_token`], with the payload
/// deliberately absent. The same token with the same payload replays the first
/// receipt; the same token with a different payload is a loud
/// `StoreError::IdempotencyConflict`; distinct tokens always write, even for
/// identical rows under one `DatasetId`.
///
/// `dataset_id` stays in the signature for back-compatibility and is a **logical**
/// multi-write identity, not part of the key. Identity-aware callers — a benchmark
/// re-run, a backfill, and later the server ledger's `IngestBatch.key` once the
/// seal routing of #768 lands — pass a fresh token per call.
///
/// A gate error is logged at `tracing::debug!` and never propagated, as in the
/// shared body.
///
/// # Errors
///
/// As [`SegmentStore::ingest`].
pub async fn ingest_batch_with_gate(store: &SegmentStore, aspect_id: &AspectId, _dataset_id: &DatasetId, idempotency_token: &str, rows: &[(i64, BigDecimal)], squash_target_rows: usize) -> Result<Vec<TxId>> {
	let idem = idempotency_key_for_token(idempotency_token);
	ingest_batch_under_key(store, aspect_id, &idem, rows, squash_target_rows).await
}

/// Ingest one legacy batch under the idempotency key derived from its payload.
///
/// This is the wrapper every legacy facade call site uses, and its behaviour is
/// the replay unit `343d626` (#485) decided: an identical `(dataset_id, rows)`
/// call replays the first receipt instead of storing the batch twice, which is
/// what protects a blind client retry from doubling rows. It cannot tell that
/// retry from an identical genuinely-new submission — only the caller can — so an
/// identical re-run through this wrapper is still absorbed. That is the residual
/// gap of #803: closing it needs a caller identity to reach this path, and callers
/// that need per-call semantics use [`ingest_batch_with_gate`].
///
/// # Errors
///
/// As [`ingest_batch_with_gate`].
pub async fn ingest_batch(store: &SegmentStore, aspect_id: &AspectId, dataset_id: &DatasetId, rows: &[(i64, BigDecimal)]) -> Result<Vec<TxId>> {
	let idem = idempotency_key(dataset_id, rows);
	ingest_batch_under_key(store, aspect_id, &idem, rows, SQUASH_TARGET_ROWS).await
}

/// Every row of a seal-backed aspect in `[start, end]`, sorted by timestamp.
///
/// The bounds are millis, inclusive, and an open end is the store's extreme. Ingest order is
/// kept among equal timestamps: the order rows mode's `ORDER BY timestamp` reads produced.
///
/// Null rows never occur in legacy data and are skipped if a frame somehow holds one.
///
/// # Errors
///
/// As [`SegmentStore::read_time_range`].
pub async fn read_rows(store: &SegmentStore, aspect_id: &AspectId, start: Option<i64>, end: Option<i64>) -> Result<Vec<(i64, BigDecimal)>> {
	let key = aspect_key(aspect_id);
	if store.schema_for(&key).await?.is_none() {
		return Ok(Vec::new());
	}
	let (timestamps, values) = store.read_time_range(&key, start.unwrap_or(i64::MIN), end.unwrap_or(i64::MAX)).await?;
	let mut rows: Vec<(i64, BigDecimal)> = timestamps.into_iter().zip(values).filter_map(|(ts, value)| value.map(|value| (ts, value))).collect();
	rows.sort_by_key(|(ts, _)| *ts);
	Ok(rows)
}

/// Rows as legacy [`Measurement`]s (see the module documentation for how
/// their ids and dataset id are synthesised).
///
/// # Errors
///
/// As [`read_rows`].
pub async fn read_measurements(store: &SegmentStore, aspect_id: &AspectId, start: Option<i64>, end: Option<i64>) -> Result<Vec<Measurement>> {
	let key = aspect_key(aspect_id);
	let rows = read_rows(store, aspect_id, start, end).await?;
	let mut seen: std::collections::HashMap<(i64, String), usize> = std::collections::HashMap::new();
	let mut measurements = Vec::with_capacity(rows.len());
	for (timestamp_millis, value) in rows {
		let occurrence = {
			let entry = seen.entry((timestamp_millis, value.to_plain_string())).or_insert(0);
			let occurrence = *entry;
			*entry += 1;
			occurrence
		};
		let timestamp = chrono::DateTime::from_timestamp_millis(timestamp_millis).ok_or_else(|| anyhow::anyhow!("seal-backed aspect {aspect_id} holds a timestamp outside the representable range: {timestamp_millis}"))?;
		measurements.push(Measurement::new(synthetic_measurement_id(&key, timestamp_millis, &value, occurrence), DatasetId::from_uuid(Uuid::nil()), timestamp, value));
	}
	Ok(measurements)
}

/// A minimal SHA-1 (FIPS 180-1), just enough for [`uuid_v5`].
struct Sha1 {
	state: [u32; 5],
	len: u64,
	buf: [u8; 64],
	buf_len: usize,
}

impl Sha1 {
	const fn new() -> Self {
		Self { state: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0], len: 0, buf: [0; 64], buf_len: 0 }
	}

	fn update(&mut self, mut data: &[u8]) {
		self.len = self.len.wrapping_add(data.len() as u64);
		if self.buf_len > 0 {
			let take = (64 - self.buf_len).min(data.len());
			self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
			self.buf_len += take;
			data = &data[take..];
			if self.buf_len == 64 {
				let block = self.buf;
				self.compress(&block);
				self.buf_len = 0;
			}
		}
		while data.len() >= 64 {
			let (block, rest) = data.split_at(64);
			let mut owned = [0u8; 64];
			owned.copy_from_slice(block);
			self.compress(&owned);
			data = rest;
		}
		if !data.is_empty() {
			self.buf[..data.len()].copy_from_slice(data);
			self.buf_len = data.len();
		}
	}

	fn finish(mut self) -> [u8; 20] {
		let bit_len = self.len.wrapping_mul(8);
		self.update(&[0x80]);
		while self.buf_len != 56 {
			self.update(&[0]);
		}
		// Append the length by hand: `update` would count it.
		self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
		let block = self.buf;
		self.compress(&block);
		let mut out = [0u8; 20];
		for (i, word) in self.state.iter().enumerate() {
			out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
		}
		out
	}

	fn compress(&mut self, block: &[u8; 64]) {
		let mut schedule = [0u32; 80];
		for i in 0..16 {
			schedule[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
		}
		for i in 16..80 {
			schedule[i] = (schedule[i - 3] ^ schedule[i - 8] ^ schedule[i - 14] ^ schedule[i - 16]).rotate_left(1);
		}
		let [mut h0, mut h1, mut h2, mut h3, mut h4] = self.state;
		for (i, &word) in schedule.iter().enumerate() {
			let (mix, round_const) = match i {
				0..=19 => ((h1 & h2) | ((!h1) & h3), 0x5A82_7999u32),
				20..=39 => (h1 ^ h2 ^ h3, 0x6ED9_EBA1),
				40..=59 => ((h1 & h2) | (h1 & h3) | (h2 & h3), 0x8F1B_BCDC),
				_ => (h1 ^ h2 ^ h3, 0xCA62_C1D6),
			};
			let tmp = h0.rotate_left(5).wrapping_add(mix).wrapping_add(h4).wrapping_add(round_const).wrapping_add(word);
			h4 = h3;
			h3 = h2;
			h2 = h1.rotate_left(30);
			h1 = h0;
			h0 = tmp;
		}
		self.state[0] = self.state[0].wrapping_add(h0);
		self.state[1] = self.state[1].wrapping_add(h1);
		self.state[2] = self.state[2].wrapping_add(h2);
		self.state[3] = self.state[3].wrapping_add(h3);
		self.state[4] = self.state[4].wrapping_add(h4);
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("decimal")
	}

	#[test]
	fn uuid_v5_matches_the_rfc_4122_shape() {
		// The Python `uuid.uuid5(uuid.NAMESPACE_URL, "python.org")` value.
		assert_eq!(uuid_v5(b"python.org").to_string(), "7af94e2b-4dd9-50f0-9c9a-8a48519bdef0");
		assert_eq!(uuid_v5(b"python.org"), uuid_v5(b"python.org"), "deterministic");
		assert_ne!(uuid_v5(b"python.org"), uuid_v5(b"example.org"));
	}

	#[test]
	fn storage_mode_parsing_and_creation_rules() {
		assert_eq!(StorageMode::parse(Some("segments")), StorageMode::Segments);
		assert_eq!(StorageMode::parse(Some("rows")), StorageMode::Rows);
		assert_eq!(StorageMode::parse(None), StorageMode::Rows, "a pre-S19 row is rows mode");
		assert_eq!(StorageMode::parse(Some("bogus")), StorageMode::Rows);
		assert_eq!(StorageMode::for_new_aspect(false, Some("segments")), StorageMode::Segments);
		assert_eq!(StorageMode::for_new_aspect(true, Some("segments")), StorageMode::Rows, "compression stays rows mode");
		// #768 flipped the unset case: a new aspect is seal-backed unless told
		// otherwise. This is the assertion that fails on a pre-flip tree, and the
		// reason the flip needs no migration — only the mode of an aspect that does
		// not exist yet changes.
		assert_eq!(StorageMode::for_new_aspect(false, None), StorageMode::Segments, "a new aspect is seal-backed unless told otherwise");
		assert_eq!(StorageMode::for_new_aspect(true, None), StorageMode::Rows, "compression still forces rows mode");
		assert_eq!(StorageMode::for_new_aspect(false, Some("rows")), StorageMode::Rows, "rows mode is now the explicit opt-out");
		assert_eq!(StorageMode::for_new_aspect(false, Some("segment")), StorageMode::Rows, "a typo keeps the shape every released build can read");
	}

	#[test]
	fn idempotency_key_names_dataset_crc_and_count() {
		let dataset = DatasetId::from_uuid(Uuid::nil());
		let rows = vec![(1_000, bd("1.5")), (2_000, bd("-2"))];
		let key = idempotency_key(&dataset, &rows);
		let digest = payload_digest(&rows);
		let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
		assert_eq!(key, format!("legacy:{}:{hex}:2", Uuid::nil()));
		assert_ne!(key, idempotency_key(&dataset, &[(1_000, bd("1.5"))]), "a different batch derives a different key");
		assert_eq!(synthetic_tx_id(&key, 0), synthetic_tx_id(&key, 0), "a retry derives the same TxIds");
		assert_ne!(synthetic_tx_id(&key, 0), synthetic_tx_id(&key, 1));
	}

	#[test]
	fn idempotency_key_for_token_names_the_call_not_the_payload() {
		// Two tokens over an identical payload give different keys: the call is the
		// unit, so a re-run is never absorbed as a replay (#803).
		let rows = vec![(1_000, bd("1.5")), (2_000, bd("-2"))];
		let key1 = idempotency_key_for_token("tok-1");
		let key2 = idempotency_key_for_token("tok-2");
		assert_eq!(key1, "legacy:tok-1");
		assert_ne!(key1, key2, "different tokens ⇒ different keys, identical payload or not");
		assert_ne!(key1, idempotency_key(&DatasetId::from_uuid(Uuid::nil()), &rows), "the two key styles do not collide");
		assert_eq!(synthetic_tx_id(&key1, 0), synthetic_tx_id(&key1, 0), "a retry of one token derives the same TxIds");
	}
}
