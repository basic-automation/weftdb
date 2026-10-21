//! Reading a frame by what the **frame** says it is, and keeping its fault class
//! attached to the failure (1.0 plan X-FRAME).
//!
//! Every frame-decoding call site in [`crate::SegmentStore`] used to choose its reader
//! from `descriptor.format_version`, a u16 copied into the index row when the frame was
//! sealed. The frame registry (5 = single-block, 6 = paged, 7 = envelope) makes the file
//! the authority on its own layout, and the index copy is at best a cache of what the
//! writer decided years ago. It is also the least protected number in the path:
//! `types/segment_index.rs` reads it out of a damaged row with `unwrap_or(0)`, which
//! quietly routes a paged frame to the single-block reader and reports a corrupt file as
//! a version problem.
//!
//! So the layout is probed from the frame's own header — magic plus the version fields,
//! nothing else — and the reader that gets chosen verifies every byte after that. A
//! header that has been damaged still fails, and fails on the frame's own checksum,
//! which is the honest answer whichever way the routing guess went.
//!
//! What travels out of here is deliberately two things, not one:
//!
//! - the **operator-visible sentence**, byte-for-byte what these sites have always
//!   produced (`decoding paged segment <path>: <decoder detail>`). Corrupt-frame triage
//!   is done by reading logs, and a slice that changes how a fault is *classified* has no
//!   business also changing what an operator greps for. When the header cannot even say
//!   which layout it is, the sentence uses the unmarked word `segment`, which is what the
//!   not-paged case has always said.
//! - a **typed fault** underneath it ([`FrameFaultError`]), carrying the fault class the
//!   plan's C5 policy is written against, so `weft-server` can answer with the status the
//!   condition deserves rather than deriving one from the sentence — which is the rule
//!   X-ERR established: read the type under the `anyhow` context, not the context's words.

use std::fmt;

use anyhow::Result;
use bigdecimal::BigDecimal;
use weft_physical_type::{frame_fault_of, frame_layout_of, read_frame_paged_segment, read_frame_point, read_frame_points, read_frame_range, read_frame_range_physical, read_frame_segment, FrameFault, FrameLayout, PagedSegment, PhysicalValue, Segment, WeftSegError};

/// A frame that this build could not decode, tagged with why in the vocabulary the
/// storage policy is written against.
///
/// The class, not the decoder's phrasing, is what the storage layer's behaviour and the
/// HTTP status follow:
///
/// - [`FrameFault::Integrity`] — damage: a checksum that does not match, a length that
///   does not add up, an envelope that contradicts the frame inside it. The file is
///   corrupt and stays corrupt in every build this one could be updated to.
/// - [`FrameFault::Undecodable`] — this build is the problem, not the file. Either it is
///   older than the frame (`newer_format`, which is a reason to refuse the store rather
///   than to quarantine a file), or the frame needs something this build does not
///   implement, such as a critical extension or a codec behind an off-by-default feature.
///
/// It sits under an `anyhow::Context` whose sentence is the historical one, so nothing
/// that reads the message changes while the type becomes available to anyone downcasting.
#[derive(Debug)]
pub struct FrameFaultError {
	/// The C5 fault class this failure belongs to.
	fault: FrameFault,
	/// The decoder's own complaint, kept so the log line and `source()` stay informative.
	source: WeftSegError,
}

impl FrameFaultError {
	/// The class a caller acts on.
	#[must_use]
	pub fn fault(&self) -> FrameFault {
		self.fault
	}

	/// The decoder's own complaint.
	#[must_use]
	pub fn source(&self) -> &WeftSegError {
		&self.source
	}
}

impl fmt::Display for FrameFaultError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		// The class leads, because this is the error a caller reaches for when the
		// surrounding context has already been read past.
		write!(formatter, "{} frame could not be decoded: {}", self.fault.fault_class(), self.source)
	}
}

impl std::error::Error for FrameFaultError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}

/// A whole frame, decoded, in whichever layout the frame itself declared.
#[derive(Debug)]
pub(crate) enum FullFrame {
	/// Layout 5.
	SingleBlock(Segment),
	/// Layout 6.
	Paged(PagedSegment),
}

impl FullFrame {
	/// Rows per page, for the index columns that only mean something to a paged frame.
	pub(crate) fn rows_per_page(&self) -> Option<usize> {
		match self {
			Self::SingleBlock(_) => None,
			Self::Paged(segment) => Some(segment.rows_per_page),
		}
	}

	/// Every row in the frame, nulls kept.
	pub(crate) fn decode_nullable(self) -> (Vec<i64>, Vec<Option<BigDecimal>>) {
		match self {
			Self::SingleBlock(segment) => segment.decode_nullable(),
			Self::Paged(segment) => segment.decode_nullable(),
		}
	}
}

/// Turns a decoder failure into the chain the store has always returned, with the typed
/// fault added underneath it.
fn decode_error(error: WeftSegError, path: &str, paged: bool) -> anyhow::Error {
	// Kept before `error` moves: the historical sentence is the point of this function.
	let shown = error.to_string();
	let fault = frame_fault_of(&error);
	let word = if paged { "paged segment" } else { "segment" };
	anyhow::Error::new(FrameFaultError { fault, source: error }).context(format!("decoding {word} {path}: {shown}"))
}

/// Whether the frame calls itself paged, for the sentence in its error path.
fn frame_is_paged(bytes: &[u8]) -> bool {
	matches!(frame_layout_of(bytes), Ok(FrameLayout::Paged))
}

/// Decode a frame and return the physical (encoded) values in a time range.
pub(crate) fn read_range_physical(path: &str, bytes: &[u8], start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<PhysicalValue>>)> {
	let paged = frame_is_paged(bytes);
	read_frame_range_physical(bytes, start, end).map_err(|error| decode_error(error, path, paged))
}

/// Decode a frame and return the values in a time range.
pub(crate) fn read_range(path: &str, bytes: &[u8], start: i64, end: i64) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>), anyhow::Error> {
	let paged = frame_is_paged(bytes);
	read_frame_range(bytes, start, end).map_err(|error| decode_error(error, path, paged))
}

/// Decode a frame and look up one timestamp.
pub(crate) fn read_point(path: &str, bytes: &[u8], t: i64) -> Result<Option<BigDecimal>, anyhow::Error> {
	let paged = frame_is_paged(bytes);
	read_frame_point(bytes, t).map_err(|error| decode_error(error, path, paged))
}

/// Decode a frame and look up several timestamps.
pub(crate) fn read_points(path: &str, bytes: &[u8], timestamps: &[i64]) -> Result<Vec<Option<BigDecimal>>, anyhow::Error> {
	let paged = frame_is_paged(bytes);
	read_frame_points(bytes, timestamps).map_err(|error| decode_error(error, path, paged))
}

/// Decode a whole frame, keeping its layout so the caller can report rows per page.
pub(crate) fn read_full(path: &str, bytes: &[u8]) -> Result<FullFrame> {
	match frame_layout_of(bytes) {
		Ok(FrameLayout::Paged) => read_frame_paged_segment(bytes).map(FullFrame::Paged).map_err(|error| decode_error(error, path, true)),
		Ok(FrameLayout::SingleBlock) => read_frame_segment(bytes).map(FullFrame::SingleBlock).map_err(|error| decode_error(error, path, false)),
		// The header could not name a layout this build knows. There is no paged/non-paged
		// answer to make, and the unmarked word is the historical default.
		Err(error) => Err(decode_error(error, path, false)),
	}
}

#[cfg(test)]
mod tests {
	use bigdecimal::BigDecimal;
	use weft_physical_type::{crc32, wrap_frame_envelope, write_paged_segment, FrameExtension, FrameFault, PagedSegment, TimeUnit, FRAME_VERSION_MAX_KNOWN};

	use super::*;

	/// A ten-row paged frame in the layout most stores actually use.
	fn paged_frame() -> PagedSegment {
		let timestamps: Vec<i64> = (0..10).collect();
		let values: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		PagedSegment::build(&timestamps, &values, TimeUnit::Nanos, &BigDecimal::from(0), 4).expect("ten rows in pages of four build")
	}

	/// The bytes of that frame, unwrapped.
	fn paged_bytes() -> Vec<u8> {
		write_paged_segment(&paged_frame())
	}

	/// Rewrite a frame's own version field and re-stamp its trailer, so a reader reaches
	/// the version check instead of tripping the checksum on the way to it.
	fn stamp_version(bytes: &mut [u8], version: u16) {
		// `MAGIC` is 7 bytes and the version is the u16 straight after it; the trailer is
		// the last 4 bytes and covers everything before it.
		bytes[7..9].copy_from_slice(&version.to_le_bytes());
		let body = bytes.len() - 4;
		let checksum = crc32(&bytes[..body]);
		bytes[body..].copy_from_slice(&checksum.to_le_bytes());
	}

	#[test]
	fn a_frame_is_read_from_what_the_file_says() {
		let body = paged_bytes();
		let wrapped = wrap_frame_envelope(&body, &[]).expect("an envelope around a real frame");
		let frame = read_full("price-1.weftseg", &wrapped).expect("the envelope's own header says which reader to use");
		assert_eq!(frame.rows_per_page(), Some(4), "the wrapped frame is still the paged one");
		let (timestamps, values) = frame.decode_nullable();
		assert_eq!(timestamps.len(), 10);
		assert!(values.iter().all(Option::is_some), "no row was lost to the envelope");
	}

	#[test]
	fn damage_keeps_the_operator_sentence_and_adds_a_fault_class() {
		let mut bytes = paged_bytes();
		// One byte in the middle: away from the magic, away from the trailer, so the
		// checksum is what complains.
		let middle = bytes.len() / 2;
		bytes[middle] ^= 0xff;
		let error = read_range("/var/lib/weft/segments/price-1.weftseg", &bytes, i64::MIN, i64::MAX).expect_err("a damaged frame does not decode");
		let fault = error.downcast_ref::<FrameFaultError>().expect("the fault class travels under the context");
		assert!(matches!(fault.fault(), FrameFault::Integrity), "damage is damage: {fault:?}");
		// The sentence an operator greps for is the one these sites have always printed.
		assert!(error.to_string().starts_with("decoding paged segment /var/lib/weft/segments/price-1.weftseg: "), "{error}");
	}

	#[test]
	fn a_frame_written_by_a_newer_build_is_the_binary_being_behind() {
		let mut bytes = paged_bytes();
		stamp_version(&mut bytes, FRAME_VERSION_MAX_KNOWN + 1);
		let error = read_range("price-1.weftseg", &bytes, i64::MIN, i64::MAX).expect_err("this build has no reader for that version");
		let fault = error.downcast_ref::<FrameFaultError>().expect("the fault class travels under the context");
		assert!(matches!(fault.fault(), FrameFault::Undecodable { newer_format: true }), "a newer frame is not a corrupt file: {fault:?}");
	}

	#[test]
	fn an_envelope_wrapping_a_body_this_build_cannot_read_is_still_newer_format() {
		// The case the envelope exists for: a later server wraps a body this build has no
		// reader for. `read_full` must not then double-check the CRC of a frame it has
		// already refused, which would report the same file as damaged.
		let mut envelope = wrap_frame_envelope(&paged_bytes(), &[]).expect("an envelope around a real frame");
		let outer = envelope.len() - 4;
		let inner = envelope[1..outer].windows(7).position(|w| w == b"WEFTSEG").expect("an envelope wraps a frame") + 1;
		let bumped = FRAME_VERSION_MAX_KNOWN + 1;
		envelope[7..9].copy_from_slice(&bumped.to_le_bytes());
		envelope[inner + 7..inner + 9].copy_from_slice(&bumped.to_le_bytes());
		let inner_crc = crc32(&envelope[inner..outer]);
		envelope[outer - 4..outer].copy_from_slice(&inner_crc.to_le_bytes());
		let outer_crc = crc32(&envelope[..outer]);
		envelope[outer..].copy_from_slice(&outer_crc.to_le_bytes());
		let error = read_full("price-9.weftseg", &envelope).expect_err("the body layout has no reader here");
		let fault = error.downcast_ref::<FrameFaultError>().expect("the fault class travels under the context");
		assert!(matches!(fault.fault(), FrameFault::Undecodable { newer_format: true }), "{fault:?}");
	}

	#[test]
	fn a_critical_extension_this_build_lacks_is_a_refusal_not_damage() {
		// The frame is intact and this build is simply not allowed to serve it: decoding
		// the body without the extension would answer with data the writer never meant.
		let wrapped = wrap_frame_envelope(&paged_bytes(), &[FrameExtension { id: 9001, critical: true, payload: vec![7] }]).expect("the envelope does not require a number this build knows");
		let error = read_full("price-1.weftseg", &wrapped).expect_err("a critical extension this build cannot apply stops the read");
		let fault = error.downcast_ref::<FrameFaultError>().expect("the fault class travels under the context");
		assert!(matches!(fault.fault(), FrameFault::Undecodable { newer_format: false }), "{fault:?}");
	}

	#[test]
	fn a_non_critical_extension_is_ignored_and_the_frame_reads() {
		let wrapped = wrap_frame_envelope(&paged_bytes(), &[FrameExtension { id: 9002, critical: false, payload: vec![7, 7] }]).expect("an extension this build does not know may still be written");
		let frame = read_full("price-1.weftseg", &wrapped).expect("an optional extension does not stop a read");
		assert_eq!(frame.decode_nullable().0.len(), 10);
	}
}
