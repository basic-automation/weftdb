//! Startup recovery and the C5 fault policy (1.0 plan §4 C5, §5 D-S12).
//!
//! A frame read fails in two classes, and the store must not treat them the same:
//!
//! - **Corrupt** ([`QuarantineClass::Corrupt`]): the bytes disagree with themselves —
//!   a checksum that does not match, a length that does not add up, a file that is
//!   not there. The file is damage, so it is quarantined and, once its TTL has
//!   passed, purged automatically.
//! - **Suspect** ([`QuarantineClass::Suspect`]): the file is internally consistent
//!   but this build cannot read it — a version it predates, a codec compiled out,
//!   an unknown critical extension, an unknown codec tag, or any other failure on a
//!   frame whose checksum verified. The file holds data another build can read, so
//!   it is removed from reads, responses carry `Weft-Incomplete`, and it is never
//!   purged automatically: only an operator releases or purges it.
//!
//! The classification rule [`classify_decode_failure`] is the plan's text, applied
//! literally: a failure on a frame whose trailing CRC-32 verifies is post-CRC, hence
//! suspect, whatever the decoder complained about; a failure on a frame whose CRC
//! does not verify is suspect only for the causes C5 names outright (a newer
//! version, a codec this build lacks, an unknown critical extension, an unknown
//! codec tag), and corrupt otherwise. That differs from [`FrameFault`](weft_physical_type::FrameFault)
//! in one named case: X-FRAME counts an unknown tag as integrity (a tag arrives with
//! a new body version), while C5 names "unknown codec tag" among the suspect causes,
//! so quarantine follows C5 here and the read path keeps X-FRAME's mapping.
//!
//! A quarantined segment is gone from reads the moment its index row is deleted, in
//! the same transaction that records it; its file moves under `segments/quarantine/`
//! (the only directory recovery writes, per D-S10), so a later open never serves it
//! again. [`RecoveryReport`] is counts only: `/ready` carries it, never the rows.

use serde::{Deserialize, Serialize};
use weft_physical_type::{crc32, SegmentDescriptor, WeftSegError};

/// The server's policy for what happens to a frame whose decode fails in the
/// background — a maintenance pass, a compaction, a read that finds a frame it
/// cannot open (ROB-22). The policy sits next to
/// [`classify_decode_failure`](Self::classify_decode_failure) so both the read path
/// and the daemon use one rule, configured at startup from
/// `WEFT_SUSPECT_FRAME_POLICY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspectFramePolicy {
	/// Serve the store without the frame, count the event, and never quarantine.
	Skip,
	/// Quarantine the frame under its C5 class (corrupt or suspect), following
	/// the existing quarantine path.
	Quarantine,
	/// Propagate the decode error to the caller, as before ROB-22.
	Fail,
}

impl std::fmt::Display for SuspectFramePolicy {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::Skip => "skip",
			Self::Quarantine => "quarantine",
			Self::Fail => "fail",
		})
	}
}

impl SuspectFramePolicy {
	/// The environment variable that names this policy.
	pub const ENV_NAME: &'static str = "WEFT_SUSPECT_FRAME_POLICY";

	/// Parse the policy from its configuration text (case-insensitive),
	/// returning `None` for anything unrecognised.
	#[must_use]
	pub fn parse(text: &str) -> Option<Self> {
		match text.trim().to_lower() {
			"skip" => Some(Self::Skip),
			"quarantine" => Some(Self::Quarantine),
			"fail" => Some(Self::Fail),
			_ => None,
		}
	}
}

/// How long a corrupt quarantine record lives before
/// [`SegmentStore::purge_expired_quarantine`](crate::SegmentStore::purge_expired_quarantine) may delete
/// it.
///
/// Thirty days, counted from when the record was written. The plan names the TTL but no duration;
/// thirty days is this slice's choice, recorded in DECISIONS.md for merge review, and long enough that
/// an operator triaging a bad seal has weeks, not hours, before the file goes.
pub const QUARANTINE_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The C5 class of a quarantined segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineClass {
	/// Integrity failure: damage. TTL purge applies.
	Corrupt,
	/// Post-CRC or named-undecodable failure: data another build can read. No
	/// automatic purge, ever.
	Suspect,
}

impl QuarantineClass {
	/// The class name the `segment_quarantine.class` column and `/ready` carry.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Corrupt => "corrupt",
			Self::Suspect => "suspect",
		}
	}

	/// Parse [`as_str`](Self::as_str) back; `None` for anything else.
	#[must_use]
	pub fn parse(text: &str) -> Option<Self> {
		match text {
			"corrupt" => Some(Self::Corrupt),
			"suspect" => Some(Self::Suspect),
			_ => None,
		}
	}
}

impl std::fmt::Display for QuarantineClass {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(self.as_str())
	}
}

/// One row of `segment_quarantine` (plan §5 D-S12: `{class, min_ts, max_ts,
/// series_id}` plus the pre-existing identity and reason columns).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRecord {
	/// The aspect the segment belonged to.
	pub aspect: String,
	/// The segment id its index row carried.
	pub id: u64,
	/// The segment generation its index row carried.
	pub gen: u64,
	/// The file name under `segments/quarantine/` (or the indexed name, when the
	/// file itself was missing and there was nothing to move).
	pub name: String,
	/// The C5 class.
	pub class: QuarantineClass,
	/// Why it was quarantined, in the decoder's or the scanner's words.
	pub reason: String,
	/// The segment's time span, from its index row, so an operator can tell what
	/// data the quarantine removed without opening the file.
	pub min_ts: Option<i64>,
	/// The segment's time span, from its index row.
	pub max_ts: Option<i64>,
	/// The tag series the segment's frame holds (tags A7; 0 is the empty tag set).
	pub series_id: u64,
	/// Milliseconds since the Unix epoch when the record was written; the TTL
	/// counts from here.
	pub quarantined_ms: i64,
	/// The quarantined row itself, serialized, so an operator release can put the
	/// segment back exactly as it was. `None` for records with no row behind them
	/// (crash litter with no index row).
	pub descriptor_json: Option<String>,
}

/// The serializable form of an index row a quarantine record carries back for a
/// release: the descriptor plus the row fields a re-insert needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantinedRow {
	/// The segment descriptor, as the index row held it.
	pub desc: SegmentDescriptor,
	/// The generation the index row carried.
	pub gen: u64,
	/// The adoption-order key the index row carried.
	pub prec: Option<u64>,
	/// The frame CRC the index row carried, if one was bound.
	pub frame_crc: Option<u32>,
	/// The commit epoch the index row carried, if any.
	pub commit_epoch: Option<u64>,
	/// The tag series the index row carried.
	pub series_id: u64,
}

impl QuarantinedRow {
	/// Serialize for the quarantine row's `descriptor_json` column.
	#[must_use]
	pub fn to_json(&self) -> String {
		serde_json::to_string(self).unwrap_or_default()
	}

	/// Parse back; `None` when the stored JSON is not a row this build wrote.
	#[must_use]
	pub fn from_json(json: &str) -> Option<Self> {
		serde_json::from_str(json).ok()
	}
}

/// What one recovery pass — at startup, or behind the fsck endpoint — found and
/// did. Counts only, so `/ready` can carry it without leaking segment contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecoveryReport {
	/// Index rows examined.
	pub checked: usize,
	/// Rows moved to corrupt quarantine (file moved or, for a missing file, row
	/// deleted with a record written).
	pub quarantined: usize,
	/// Rows moved to suspect quarantine.
	pub suspected: usize,
	/// Rows re-bound to their frame's key (see [`SegmentStore::fsck`](crate::SegmentStore::fsck)).
	pub rebound: usize,
	/// `aspect_seq.next_series_id` rows seeded (R10).
	pub seeded: usize,
	/// Quarantine records purged by this pass (TTL expiry, or an operator purge
	/// counted by its own call, not here).
	pub purged: usize,
	/// Files under `segments/` no index row or journal entry claims, counted
	/// whether or not the pass quarantined them.
	pub orphans: usize,
	/// Orphan frames a restore-mode pass adopted into the index (D-S16, R8),
	/// plus held frames a later pass released into it (TAG-9).
	pub adopted: usize,
	/// Orphan frames a restore-mode pass journaled `held`: they carry a
	/// `SERIES_KEY` extension and their aspect has no declared schema, so
	/// there is no catalog to bind the key in yet (D-S16, TAG-9). A later pass
	/// adopts them once the aspect is declared.
	pub held: usize,
	/// Rows whose series id was rebound to the series their frame's
	/// `SERIES_KEY` names (TAG-9): the catalog key under the row's id differed
	/// from the frame's, or the row moved to another aspect and its key was
	/// resolved again there. Each is also counted in `rebound`.
	pub series_rebound: usize,
	/// Held journal rows cleared without an adoption (TAG-9): the held frame
	/// was missing, held no rows, or named no aspect, so nothing could ever
	/// adopt it.
	pub held_cleared: usize,
	/// Rows whose indexed path escapes `segments/` and were left exactly where
	/// they are: recovery never moves a file it cannot contain.
	pub skipped_outside_segments: usize,
	/// Whether any suspect record's cause is a frame version this build predates:
	/// `/ready` reports `incompatible_store` exactly then.
	pub incompatible_store: bool,
	/// Whether the pass only reported: a newer-layout store, where recovery must
	/// not quarantine, adopt or purge anything a newer WeftDB might understand.
	pub report_only: bool,
}

impl RecoveryReport {
	/// The counts `/ready` carries, as a single total per class.
	#[must_use]
	pub const fn quarantined_total(&self) -> usize {
		self.quarantined + self.suspected
	}
}

/// Whether `bytes` end in a CRC-32 trailer that matches the body before it.
///
/// The plain v5/v6 frame layout and the v7 envelope both close with a little-endian CRC-32 over
/// everything before it, so one check covers both.
#[must_use]
pub fn crc_verified(bytes: &[u8]) -> bool {
	if bytes.len() < 11 {
		return false;
	}
	let (body, trailer) = bytes.split_at(bytes.len() - 4);
	let stored = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
	stored == crc32(body)
}

/// Classify a frame-decode failure under C5, given the bytes that failed.
///
/// A frame whose CRC verifies failed after the CRC, so it is suspect whatever the
/// decoder said. Otherwise the causes C5 names outright are suspect, and every
/// other failure — checksum, length, magic, malformed structure on a frame whose
/// CRC does not verify — is corrupt.
#[must_use]
pub fn classify_decode_failure(bytes: &[u8], error: &WeftSegError) -> QuarantineClass {
	if crc_verified(bytes) {
		return QuarantineClass::Suspect;
	}
	match error {
		WeftSegError::UnsupportedVersion { .. } | WeftSegError::CodecNotEnabled { .. } | WeftSegError::UnknownCriticalExtension { .. } | WeftSegError::InvalidTag { .. } => QuarantineClass::Suspect,
		_ => QuarantineClass::Corrupt,
	}
}

/// Whether a decode failure's cause is a frame version this build predates, the
/// only case `/ready` reports as `incompatible_store`.
#[must_use]
pub const fn is_newer_format(error: &WeftSegError) -> bool {
	matches!(error, WeftSegError::UnsupportedVersion { found } if *found > weft_physical_type::FRAME_VERSION_MAX_KNOWN)
}

/// The action the server takes when a decode failure is C5-suspect (ROB-22):
/// whether to skip the frame silently, quarantine it, or fail the caller.
/// A corrupt frame is always quarantined ([`QuarantineClass::Corrupt`]) — the
/// policy's purpose is the suspect half, where the right answer depends on
/// whether the deployment wants to keep reading or stop on an undecodable
/// frame. The policy grows from a single env var for now (ROB-22 1.) and is
/// resolved here via [`classify_decode_failure`](Self::classify_decode_failure)
/// so callers never decide on their own.
#[must_use]
pub fn decode_failure_action(bytes: &[u8], error: &WeftSegError, policy: SuspectFramePolicy) -> SuspectFramePolicy {
	if classify_decode_failure(bytes, error) == QuarantineClass::Suspect {
		// Suspect-class failure: the frame is internally sound but this build
		// cannot read it. The configured policy decides what to do.
		policy
	} else {
		// Integrity failure: the frame is damaged. Always quarantine it.
		SuspectFramePolicy::Quarantine
	}
}

#[cfg(test)]
mod tests {
	use bigdecimal::BigDecimal;
	use weft_physical_type::{write_paged_segment, PagedSegment, TimeUnit};

	use super::*;

	fn paged_bytes() -> Vec<u8> {
		let timestamps: Vec<i64> = (0..10).collect();
		let values: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let segment = PagedSegment::build(&timestamps, &values, TimeUnit::Nanos, &BigDecimal::from(0), 4).expect("ten rows build");
		write_paged_segment(&segment)
	}

	#[test]
	fn crc_verified_accepts_a_sound_frame_and_rejects_damage() {
		let bytes = paged_bytes();
		assert!(crc_verified(&bytes), "a frame this build wrote verifies");
		let mut damaged = bytes;
		let middle = damaged.len() / 2;
		damaged[middle] ^= 0xff;
		assert!(!crc_verified(&damaged), "a flipped byte breaks the trailer");
		assert!(!crc_verified(&[]), "an empty file has no trailer to verify");
	}

	#[test]
	fn a_post_crc_failure_is_suspect_even_when_the_decoder_calls_it_damage() {
		let mut bytes = paged_bytes();
		// Re-stamp the trailer after damaging a value byte, so the CRC verifies
		// while the decoded content is no longer what the writer wrote: the
		// failure, when it comes, is post-CRC by construction.
		let middle = bytes.len() / 2;
		bytes[middle] ^= 0xff;
		let body = bytes.len() - 4;
		let checksum = crc32(&bytes[..body]);
		bytes[body..].copy_from_slice(&checksum.to_le_bytes());
		assert!(crc_verified(&bytes));
		assert_eq!(classify_decode_failure(&bytes, &WeftSegError::InvalidDecimal), QuarantineClass::Suspect, "post-CRC InvalidDecimal is suspect, not corrupt");
	}

	#[test]
	fn c5_named_causes_are_suspect_without_a_verified_crc() {
		let bytes = paged_bytes();
		for error in [WeftSegError::UnsupportedVersion { found: 99 }, WeftSegError::CodecNotEnabled { codec: "x", feature: "y" }, WeftSegError::UnknownCriticalExtension { id: 7 }, WeftSegError::InvalidTag { kind: "codec", value: 0xff }] {
			assert_eq!(classify_decode_failure(&bytes, &error), QuarantineClass::Suspect, "{error:?} is a C5 suspect cause");
		}
	}

	#[test]
	fn checksum_and_length_failures_are_corrupt() {
		let bytes = paged_bytes();
		// Each case feeds bytes the error describes: the classifier trusts the
		// frame's own trailer, so sound bytes paired with a checksum error
		// would be post-CRC by construction, not corrupt.
		let mut damaged = bytes.clone();
		let middle = damaged.len() / 2;
		damaged[middle] ^= 0xff;
		assert_eq!(classify_decode_failure(&damaged, &WeftSegError::ChecksumMismatch { stored: 1, computed: 2 }), QuarantineClass::Corrupt);
		let truncated = bytes[..bytes.len() / 2].to_vec();
		assert_eq!(classify_decode_failure(&truncated, &WeftSegError::UnexpectedEof { needed: 10, remaining: 2 }), QuarantineClass::Corrupt);
		let mut bad_magic = bytes;
		bad_magic[0] ^= 0xff;
		assert_eq!(classify_decode_failure(&bad_magic, &WeftSegError::BadMagic), QuarantineClass::Corrupt);
	}

	#[test]
	fn only_a_newer_version_is_incompatible_store() {
		assert!(is_newer_format(&WeftSegError::UnsupportedVersion { found: weft_physical_type::FRAME_VERSION_MAX_KNOWN + 1 }));
		assert!(!is_newer_format(&WeftSegError::UnsupportedVersion { found: 4 }));
		assert!(!is_newer_format(&WeftSegError::CodecNotEnabled { codec: "x", feature: "y" }));
	}

	#[test]
	fn quarantine_class_round_trips_through_its_column_text() {
		for class in [QuarantineClass::Corrupt, QuarantineClass::Suspect] {
			assert_eq!(QuarantineClass::parse(class.as_str()), Some(class));
		}
		assert_eq!(QuarantineClass::parse("bogus"), None);
	}

	#[test]
	fn suspect_frame_policy_parse_accepts_every_variant() {
		for (text, expected) in [("skip", Some(SuspectFramePolicy::Skip)), ("Skip", Some(SuspectFramePolicy::Skip)), ("SKIP", Some(SuspectFramePolicy::Skip)), ("quarantine", Some(SuspectFramePolicy::Quarantine)), ("Quarantine", Some(SuspectFramePolicy::Quarantine)), ("fail", Some(SuspectFramePolicy::Fail)), ("Fail", Some(SuspectFramePolicy::Fail)), ("bogus", None), ("", None)] {
			assert_eq!(SuspectFramePolicy::parse(text), expected, "parse({text:?})");
		}
	}

	#[test]
	fn suspect_frame_policy_display_round_trips() {
		for policy in [SuspectFramePolicy::Skip, SuspectFramePolicy::Quarantine, SuspectFramePolicy::Fail] {
			let text: String = format!("{policy}");
			assert_eq!(SuspectFramePolicy::parse(text.as_str()), Some(policy), "the Display text parses back");
		}
	}

	#[test]
	fn decode_failure_action_suspect_frames_respect_the_policy() {
		let bytes = paged_bytes();
		// A post-CRC failure is suspect; each policy is honoured.
		let post_crc = {
			let mut b = bytes.clone();
			let middle = b.len() / 2;
			b[middle] ^= 0xff;
			let body = b.len() - 4;
			let checksum = crc32(&b[..body]);
			b[body..].copy_from_slice(&checksum.to_le_bytes());
			b
		};
		assert!(crc_verified(&post_crc));
		assert_eq!(decode_failure_action(&post_crc, &WeftSegError::InvalidDecimal, SuspectFramePolicy::Skip), SuspectFramePolicy::Skip);
		assert_eq!(decode_failure_action(&post_crc, &WeftSegError::InvalidDecimal, SuspectFramePolicy::Quarantine), SuspectFramePolicy::Quarantine);
		assert_eq!(decode_failure_action(&post_crc, &WeftSegError::InvalidDecimal, SuspectFramePolicy::Fail), SuspectFramePolicy::Fail);
	}

	#[test]
	fn decode_failure_action_corrupt_frames_always_quarantine() {
		let mut bytes = paged_bytes();
		let middle = bytes.len() / 2;
		bytes[middle] ^= 0xff;
		// No CRC verification: this is an integrity failure.
		assert!(!crc_verified(&bytes));
		// Every policy returns Quarantine for a corrupt frame.
		for policy in [SuspectFramePolicy::Skip, SuspectFramePolicy::Quarantine, SuspectFramePolicy::Fail] {
			assert_eq!(decode_failure_action(&bytes, &WeftSegError::ChecksumMismatch { stored: 1, computed: 2 }, policy), SuspectFramePolicy::Quarantine);
		}
	}

	#[test]
	fn decode_failure_action_c5_named_causes_are_subject_to_policy() {
		let bytes = paged_bytes();
		for error in [WeftSegError::UnsupportedVersion { found: 99 }, WeftSegError::CodecNotEnabled { codec: "x", feature: "y" }, WeftSegError::UnknownCriticalExtension { id: 7 }, WeftSegError::InvalidTag { kind: "codec", value: 0xff }] {
			for policy in [SuspectFramePolicy::Skip, SuspectFramePolicy::Quarantine, SuspectFramePolicy::Fail] {
				assert_eq!(decode_failure_action(&bytes, &error, policy), policy, "{error:?} with {policy:?} should respect the policy");
			}
		}
	}
}
