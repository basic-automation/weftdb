//! The idempotent atomic ingest entry point's data shapes (1.0 plan §5,
//! slice **D-S13**): one batch, its fingerprint, its receipt, and the phase an
//! ingest is in when something fails.
//!
//! [`SegmentStore::ingest`](crate::SegmentStore::ingest) is the entry point
//! itself. This module holds what it takes and returns:
//!
//! - [`IngestBatch`]: an idempotency `key`, a `groups` tag set, and the
//!   `(timestamp, value)` rows (nullable, like the quality-column seal).
//!   Before the tags gate flips, `groups` accepts only the empty
//!   [`TagSet`] — series 0 — exactly as the plan's D-S13 amendment and C1/A4
//!   prescribe; a non-empty set is refused with the typed
//!   [`StoreError::GroupsUnsupported`](crate::StoreError).
//! - The **fingerprint**: a deterministic 64-bit hash over the batch's rows,
//!   each present value hashed as its **bounded plain form** (ROB-6's one
//!   decimal domain: the form every sink writes, at most
//!   [`MAX_PLAIN_DIGITS`](weft_physical_type::MAX_PLAIN_DIGITS) digits). A
//!   value outside the domain is refused before its plain form is
//!   materialised — the digit count is arithmetic, never an allocation the
//!   value's exponent names. The fingerprint covers the groups' canonical
//!   series key, the row count, and every timestamp and value in order, so
//!   two batches with the same fingerprint are the same batch for the
//!   ledger's purposes, and any difference — a moved timestamp, a changed
//!   digit, a null where a value stood — is a different batch.
//! - [`IngestReceipt`]: what a committed batch returns, and what a replay of
//!   the same batch under the same key returns again (with
//!   [`duplicate`](IngestReceipt::duplicate) set). The receipt is also what
//!   the ledger row stores as `receipt_json`, so a replay after a restart
//!   answers from the ledger, not from memory.
//! - [`IngestPhase`]: where an ingest is in its commit protocol. ROB-3's
//!   panic mapping (plan C12/D21) keys off it, so the phase is part of the
//!   library surface from this slice on: it advances as the ingest does, and
//!   is set to [`Committing`](IngestPhase::Committing) just before the one
//!   index COMMIT that makes the batch durable.
//!
//! **Frame target.** A batch is split into segments at timestamp
//! boundaries so that no segment's encoded frame exceeds
//! [`MAX_ENCODED_FRAME_BYTES`] — the 128 MiB encoded-frame target (ROB
//! D12). The split never divides a run of equal timestamps, and a single
//! row whose frame alone exceeds the target still seals (a frame is the
//! smallest unit a batch can be cut into).

use std::fmt;

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use weft_physical_type::{check_decimal_domain, TagSet};

use crate::StoreError;

/// The encoded-frame target an ingest batch is cut to (ROB D12): no segment
/// an ingest seals carries an encoded frame larger than this, unless a
/// single row's frame alone exceeds it.
pub const MAX_ENCODED_FRAME_BYTES: u64 = 128 * 1024 * 1024;

/// Where an ingest is in its commit protocol (plan C12/D21).
///
/// The phases advance in declaration order. An ingest that fails reports
/// (through its error's context, and for ROB-3 through the panic hook's
/// mapping) the phase it had reached; in particular
/// [`Committing`](Self::Committing) is set just before the ingest's one
/// index COMMIT, so a failure at or after it is known to be a commit
/// failure — possibly an ambiguous one — and not a validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum IngestPhase {
	/// The batch is being checked: schema, groups, lengths, decimal domain,
	/// and the ledger lookup that decides whether this is a replay.
	Validating,
	/// The batch's rows are being encoded into frames under the aspect's
	/// declared schema.
	Encoding,
	/// The encoded frames are being written durably under `segments/`.
	WritingFrames,
	/// The frames are durable; the index transaction — segment rows, the
	/// allocator raise, the rollup fold and the ledger row — is about to
	/// COMMIT (or is committing).
	Committing,
}

impl fmt::Display for IngestPhase {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Validating => "validating",
			Self::Encoding => "encoding",
			Self::WritingFrames => "writing frames",
			Self::Committing => "committing",
		})
	}
}

/// One batch offered to [`SegmentStore::ingest`](crate::SegmentStore::ingest):
/// an idempotency key, a groups tag set, and its rows.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestBatch {
	/// The idempotency key, scoped to the aspect: the same key offered again
	/// with the same batch replays the first commit's receipt; with a
	/// different batch it is an
	/// [`IdempotencyConflict`](StoreError::IdempotencyConflict). Keys are
	/// the caller's, from whatever namespace its own retries use (the HTTP
	/// layer's `Idempotency-Key` header lands on this field in D-S14).
	pub key: String,
	/// The batch's tag groups (plan §4 C1, shape A4). Before the tags gate
	/// flips only the empty set is accepted — series 0 — and a non-empty set
	/// is refused with [`GroupsUnsupported`](StoreError::GroupsUnsupported).
	pub groups: TagSet,
	/// The rows' timestamps, in the order offered. A batch is sealed in the
	/// order given; rows are never reordered by ingest.
	pub timestamps: Vec<i64>,
	/// The rows' values, parallel to [`timestamps`](Self::timestamps):
	/// `None` is a null row (the quality-column form, as
	/// [`seal_nullable`](crate::SegmentStore::seal_nullable) takes).
	pub values: Vec<Option<BigDecimal>>,
}

impl IngestBatch {
	/// A batch of nullable rows under `key`, in the empty group.
	#[must_use]
	pub fn new(key: impl Into<String>, timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) -> Self {
		Self { key: key.into(), groups: TagSet::new(), timestamps, values }
	}

	/// A batch of dense rows under `key`, in the empty group.
	#[must_use]
	pub fn dense(key: impl Into<String>, timestamps: Vec<i64>, values: Vec<BigDecimal>) -> Self {
		Self::new(key, timestamps, values.into_iter().map(Some).collect())
	}

	/// The same batch in `groups` (refused at ingest unless the set is
	/// empty, until the tags gate flips).
	#[must_use]
	pub fn with_groups(mut self, groups: TagSet) -> Self {
		self.groups = groups;
		self
	}

	/// The series ids this batch's rows belong to: the empty group is
	/// series 0, the only series an ingest can name before the tags gate
	/// flips. Recorded on every segment row the batch commits and in the
	/// ledger's `series_ids` column (plan §4 C1).
	#[must_use]
	pub fn series_ids(&self) -> Vec<u64> {
		vec![0]
	}

	/// This batch's fingerprint: the deterministic hash the ledger records
	/// and compares (see the module documentation). Equal batches — same
	/// groups, same rows in the same order, values equal in their bounded
	/// plain form — fingerprint equal; any other difference changes it.
	///
	/// # Errors
	///
	/// [`StoreError::IngestValueOutOfRange`] when a value's plain form is
	/// outside the one decimal domain (ROB-6), decided arithmetically
	/// before any plain form is materialised.
	pub fn fingerprint(&self) -> Result<i64, StoreError> {
		let mut hash = Fnv1a::new();
		hash.bytes(self.groups.series_key().as_bytes());
		hash.u64(u64::try_from(self.timestamps.len()).unwrap_or(u64::MAX));
		for (timestamp, value) in self.timestamps.iter().zip(&self.values) {
			hash.i64(*timestamp);
			match value {
				None => hash.byte(0),
				Some(value) => {
					check_decimal_domain(value).map_err(|e| StoreError::IngestValueOutOfRange { digits: e.digits, max: e.max })?;
					hash.byte(1);
					hash.bytes(value.to_plain_string().as_bytes());
				}
			}
		}
		Ok(hash.finish())
	}
}

/// What a committed ingest returns: the ledger's record of the batch.
///
/// A replay — the same batch under the same key — returns the recorded
/// receipt unchanged except [`duplicate`](Self::duplicate), which the first
/// commit leaves `false` and every replay sets `true`. The receipt
/// serialises to the ledger row's `receipt_json`, so the answer survives a
/// restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IngestReceipt {
	/// The aspect the batch committed to.
	pub aspect: String,
	/// The idempotency key the batch was offered under.
	pub key: String,
	/// The batch's fingerprint, as recorded in the ledger.
	pub fingerprint: i64,
	/// How many rows the batch held.
	pub row_count: usize,
	/// The batch's earliest timestamp, or `None` for an empty batch (which
	/// ingest refuses, so a committed receipt always carries one).
	pub min_ts: Option<i64>,
	/// The batch's latest timestamp.
	pub max_ts: Option<i64>,
	/// The segment ids the batch's commit assigned, in time order.
	pub segment_ids: Vec<u64>,
	/// The series ids the batch's rows belong to (`[0]`, the empty group,
	/// until the tags gate flips).
	pub series_ids: Vec<u64>,
	/// The aspect epoch the batch committed in.
	pub commit_epoch: u64,
	/// Whether this receipt is a replay of an earlier commit (`true`) or
	/// that commit itself (`false`).
	pub duplicate: bool,
}

impl IngestReceipt {
	/// Serialise for the ledger's `receipt_json` column.
	pub(crate) fn to_json(&self) -> String {
		serde_json::to_string(self).unwrap_or_default()
	}

	/// Read back a receipt stored by [`to_json`](Self::to_json).
	///
	/// # Errors
	///
	/// A `serde_json` error when the stored text is not a receipt this
	/// build wrote.
	pub(crate) fn from_json(json: &str) -> Result<Self, serde_json::Error> {
		serde_json::from_str(json)
	}
}

/// One row of `ingest_ledger` (plan §4 C1): the record an ingest commit
/// writes and a replay reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LedgerEntry {
	/// The aspect the batch committed to.
	pub aspect: String,
	/// The idempotency key, the ledger's key within the aspect.
	pub key: String,
	/// The committed batch's fingerprint.
	pub fingerprint: i64,
	/// How many rows the batch held.
	pub row_count: usize,
	/// The batch's earliest timestamp.
	pub min_ts: Option<i64>,
	/// The batch's latest timestamp.
	pub max_ts: Option<i64>,
	/// The segment ids the commit assigned, lowest and highest: a batch
	/// cut into several segments holds a consecutive run of ids, so the
	/// pair names them all.
	pub id_lo: Option<u64>,
	/// See [`id_lo`](Self::id_lo).
	pub id_hi: Option<u64>,
	/// The receipt the commit returned, serialised.
	pub receipt_json: String,
	/// The series ids the batch's rows belong to, as the `series_ids`
	/// column records them (comma-joined, empty for none — a committed
	/// batch always records at least series 0).
	pub series_ids: Vec<u64>,
	/// The aspect epoch the batch committed in.
	pub commit_epoch: u64,
	/// When the ledger row was written, in milliseconds since the epoch.
	pub created_ms: i64,
}

/// FNV-1a over bytes, with a length-prefixed feed per field so that field
/// boundaries cannot be smuggled across (`[1, 23]` and `[12, 3]` differ).
/// Deterministic across builds and platforms, which the ledger needs: a
/// fingerprint recorded by one build must compare equal under the next.
struct Fnv1a(u64);

impl Fnv1a {
	fn new() -> Self {
		Self(0xcbf2_9ce4_8422_2325)
	}

	fn byte(&mut self, byte: u8) {
		self.0 ^= u64::from(byte);
		self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
	}

	fn bytes(&mut self, bytes: &[u8]) {
		self.u64(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
		for byte in bytes {
			self.byte(*byte);
		}
	}

	fn u64(&mut self, value: u64) {
		for byte in value.to_le_bytes() {
			self.byte(byte);
		}
	}

	fn i64(&mut self, value: i64) {
		self.u64(value.cast_unsigned());
	}

	fn finish(&self) -> i64 {
		self.0.cast_signed()
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn bd(text: &str) -> BigDecimal {
		BigDecimal::from_str(text).expect("decimal")
	}

	#[test]
	fn equal_batches_fingerprint_equal_and_any_difference_changes_it() {
		let batch = IngestBatch::dense("k", vec![0, 10, 20], vec![bd("1"), bd("2.5"), bd("-3")]);
		assert_eq!(batch.fingerprint().expect("in domain"), IngestBatch::dense("k", vec![0, 10, 20], vec![bd("1"), bd("2.5"), bd("-3")]).fingerprint().expect("in domain"), "the key is not part of the fingerprint: the ledger keys on it");
		assert_ne!(batch.fingerprint().unwrap(), IngestBatch::dense("k", vec![0, 10, 21], vec![bd("1"), bd("2.5"), bd("-3")]).fingerprint().unwrap(), "a moved timestamp");
		assert_ne!(batch.fingerprint().unwrap(), IngestBatch::dense("k", vec![0, 10, 20], vec![bd("1"), bd("2.5"), bd("-3.0")]).fingerprint().unwrap(), "a different plain form");
		assert_ne!(batch.fingerprint().unwrap(), IngestBatch::new("k", vec![0, 10, 20], vec![Some(bd("1")), None, Some(bd("-3"))]).fingerprint().unwrap(), "a null where a value stood");
		assert_ne!(batch.fingerprint().unwrap(), IngestBatch::dense("k", vec![10, 0, 20], vec![bd("2.5"), bd("1"), bd("-3")]).fingerprint().unwrap(), "a reordering");
		let grouped = batch.clone().with_groups(TagSet::from_pairs([("host", "a")]).expect("valid tags"));
		assert_ne!(batch.fingerprint().unwrap(), grouped.fingerprint().unwrap(), "different groups");
	}

	#[test]
	fn a_value_outside_the_decimal_domain_is_refused_before_its_plain_form_exists() {
		let batch = IngestBatch::dense("k", vec![0], vec![bd("1e1024")]);
		assert_eq!(batch.fingerprint(), Err(StoreError::IngestValueOutOfRange { digits: 1025, max: weft_physical_type::MAX_PLAIN_DIGITS }));
		// A hostile exponent is measured arithmetically, not expanded.
		let hostile = IngestBatch::dense("k", vec![0], vec![bd("1e100000000")]);
		assert!(matches!(hostile.fingerprint(), Err(StoreError::IngestValueOutOfRange { .. })));
	}

	#[test]
	fn phases_advance_in_declaration_order() {
		assert!(IngestPhase::Validating < IngestPhase::Encoding);
		assert!(IngestPhase::Encoding < IngestPhase::WritingFrames);
		assert!(IngestPhase::WritingFrames < IngestPhase::Committing);
	}
}
