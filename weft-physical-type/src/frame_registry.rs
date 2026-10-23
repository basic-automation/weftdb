//! The version registry for `.weftseg` frame layouts, and the fault classes a frame
//! read can fail with.
//!
//! Every `.weftseg` file names its own layout: the seven magic bytes, then a `u16`
//! little-endian format version. That number is the whole of what a reader needs to
//! know how to decode what follows, so it is a shared, permanent name — three tracks
//! of this project allocated frame versions independently and would have shipped
//! different meanings for the same number (1.0 plan X-FRAME). This module is where
//! the names live: [`FRAME_SINGLE_BLOCK`] and [`FRAME_PAGED`] are the two layouts
//! WeftDB has always written, [`FRAME_ENVELOPE`] is the v7 wrapper that lets a frame
//! carry extension blocks in front of a body whose bytes are already frozen.
//!
//! **A number that has shipped is never reused, redefined, or retired**, so a file
//! written by an old server still means the same thing to a new one, and a file
//! written by a new server is refused rather than misread. The list is append-only:
//! the next layout takes 8.
//!
//! # Fault classes
//!
//! A frame read can fail in two ways that the operator should not treat the same way
//! (1.0 plan resolution C5). [`FrameFault::Integrity`] means the bytes disagree with
//! their own checksum, length, or magic: the file is damaged, and the quarantine
//! machinery is allowed to purge it. [`FrameFault::Undecodable`] means the file is
//! internally consistent but this build cannot make sense of it — a version it has
//! never heard of, an extension it cannot honour, a codec it was compiled without.
//! Those files are never damaged, and deleting one destroys data that a different
//! build could read: they go suspect, not corrupt, and `newer_format: true` is the
//! sub-case where a server should also say on `/ready` that the store on disk is a
//! format it cannot open.
//!
//! The quarantine machinery itself is D-S12's (verified by TAG-9 and ROB-22); what
//! lives here is the vocabulary every layer agrees on in the meantime.

/// The single-block frame layout WeftDB has always written: one block per column, the
/// whole aspect's rows in one file.
pub const FRAME_SINGLE_BLOCK: u16 = 5;
/// The paged frame layout: fixed-size pages, each independently decodable, so a range
/// read touches only the pages it needs.
pub const FRAME_PAGED: u16 = 6;
/// The envelope: this version number's own layout, a header naming the body layout
/// plus an extension table, then a complete frame of that layout.
///
/// The envelope exists because a body layout, once written by a released server, is
/// frozen: adding a field inside it would silently shift every byte after it. A frame
/// that needs something new puts it in front of the frozen body instead, where an
/// older reader can still refuse the file cleanly rather than decode garbage from a
/// moved offset.
pub const FRAME_ENVELOPE: u16 = 7;

/// The highest frame version this build can read. Anything above it came from a newer
/// server, which is the difference between "this file is broken" and "this file is
/// ahead of me" — see [`FrameFault::Undecodable`].
pub const FRAME_VERSION_MAX_KNOWN: u16 = FRAME_ENVELOPE;

/// Which layout a frame body uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLayout {
	/// [`FRAME_SINGLE_BLOCK`].
	SingleBlock,
	/// [`FRAME_PAGED`].
	Paged,
}

/// The layout a body version names, or `None` for a version this build has no reader
/// for.
///
/// The version in a frame's own header is not necessarily a layout this build knows;
/// the caller decides whether that is an error worth reporting.
#[must_use]
pub const fn layout_of(version: u16) -> Option<FrameLayout> {
	match version {
		FRAME_SINGLE_BLOCK => Some(FrameLayout::SingleBlock),
		FRAME_PAGED => Some(FrameLayout::Paged),
		_ => None,
	}
}

/// An entry in a v7 envelope's extension table.
///
/// `critical` is the whole of the compatibility contract: an extension the reader has
/// never heard of is skipped when it is informational, and refuses the frame when it
/// is critical, because "critical" is the writer's way of saying *the body below is
/// only correct if you applied me*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameExtension {
	/// The extension's number. Extension numbers are a permanent namespace, on the
	/// same rule as frame versions: never reuse one, and never renumber one that has
	/// shipped.
	pub id: u32,
	/// Whether a reader that does not know this extension must refuse the frame.
	pub critical: bool,
	/// The extension's own bytes. The envelope does not interpret them; the extension
	/// defines its own payload format, versioned by its own number.
	pub payload: Vec<u8>,
}

/// An extension number this build implements, and what it means.
///
/// The description is not decoration: it is what the reader prints when a frame asks
/// for something the binary does not have, and an operator cannot decide between
/// "upgrade the server" and "this file is broken" without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameExtensionSpec {
	/// The extension's number.
	pub id: u32,
	/// What the extension does, in the sentence an operator reads.
	pub name: &'static str,
}

/// The extension numbers this build implements.
///
/// Append-only, on exactly the rule stated for frame versions at the top of this
/// module: an extension that has shipped keeps its number, and a new one takes the
/// next free number. It is empty because the envelope ships with no extensions — the
/// point of X-FRAME is the *mechanism* that lets a later slice put a block in front of
/// a frozen body without moving a byte inside it, and a number invented before there
/// is a writer for it is a number wasted.
pub const KNOWN_EXTENSIONS: &[FrameExtensionSpec] = &[];

/// Whether this build knows what to do with extension `id`.
///
/// A frame that names an extension absent from [`KNOWN_EXTENSIONS`] is a frame written
/// by a newer server, not a damaged one: the answer feeds
/// [`FrameFault::Undecodable`], and the file is kept.
#[must_use]
pub fn is_known_extension(id: u32) -> bool {
	KNOWN_EXTENSIONS.iter().any(|spec| spec.id == id)
}

/// What a frame is, without decoding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameInfo {
	/// The version in the frame's own header: one of [`FRAME_SINGLE_BLOCK`],
	/// [`FRAME_PAGED`], [`FRAME_ENVELOPE`], or a number this build has no reader for.
	pub version: u16,
	/// The version of the body bytes themselves — the same as [`Self::version`] for a
	/// plain v5/v6 frame, and the wrapped layout's number for an envelope.
	pub body_version: u16,
	/// The body's layout, or `None` if this build has no reader for it.
	pub layout: Option<FrameLayout>,
	/// The envelope's extension table; empty for a plain frame.
	pub extensions: Vec<FrameExtension>,
	/// Set when the frame carries a **critical** extension this build does not
	/// implement: the body is not safe to serve, because it was written on the
	/// assumption that a reader would apply what is missing.
	pub unknown_critical: bool,
}

impl FrameInfo {
	/// Whether the frame is safe to decode with this build: the body layout is known
	/// and no critical extension is being ignored.
	#[must_use]
	pub const fn decodable(&self) -> bool {
		self.layout.is_some() && !self.unknown_critical
	}
}

/// Why a frame could not be read, in the two classes the plan's C5 resolution defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFault {
	/// The bytes disagree with their own magic, checksum or length: the file is
	/// damaged. Quarantine and, under the store's own policy, TTL purge are allowed.
	Integrity,
	/// The file is internally consistent but this build cannot read it. Its contents
	/// are not damaged, so it is never purged automatically.
	Undecodable {
		/// The cause is a frame version this build predates, so the store holds data
		/// written by a newer server. `/ready` reports `incompatible_store` for this
		/// case and only this case.
		newer_format: bool,
	},
}

impl FrameFault {
	/// The fault class name, for logs and for the operator-facing quarantine reason:
	/// `integrity`, `newer_format`, or `undecodable`.
	#[must_use]
	pub const fn fault_class(&self) -> &'static str {
		match self {
			Self::Integrity => "integrity",
			Self::Undecodable { newer_format: true } => "newer_format",
			Self::Undecodable { newer_format: false } => "undecodable",
		}
	}

	/// Whether the fault means the file itself is damaged.
	#[must_use]
	pub const fn is_integrity(&self) -> bool {
		matches!(self, Self::Integrity)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_registry_numbers_are_the_ones_the_formats_use() {
		// Pinned against the constants the writers use, so a renumber here and a
		// renumber there cannot drift apart.
		assert_eq!(FRAME_SINGLE_BLOCK, crate::segment::SEGMENT_FORMAT_VERSION);
		assert_eq!(FRAME_PAGED, crate::page::PAGED_SEGMENT_FORMAT_VERSION);
		assert_eq!(FRAME_ENVELOPE, 7);
		assert_eq!(FRAME_VERSION_MAX_KNOWN, FRAME_ENVELOPE);
	}

	#[test]
	fn layout_of_names_only_what_this_build_can_read() {
		assert_eq!(layout_of(FRAME_SINGLE_BLOCK), Some(FrameLayout::SingleBlock));
		assert_eq!(layout_of(FRAME_PAGED), Some(FrameLayout::Paged));
		for unknown in [0, 1, 2, 3, 4, FRAME_ENVELOPE, 8, u16::MAX] {
			assert_eq!(layout_of(unknown), None, "{unknown} has no reader in this build");
		}
	}

	#[test]
	fn no_extension_number_is_claimed_yet() {
		// The envelope mechanism is what X-FRAME ships; the extensions come later, one
		// per slice that needs a block in front of a frozen body. This assertion is the
		// place where a new extension becomes visible instead of incidental.
		assert!(KNOWN_EXTENSIONS.is_empty());
		for id in 0..=16 {
			assert!(!is_known_extension(id), "extension {id} has no implementation in this build");
		}
		// Numbers are a permanent namespace: a table may only ever gain distinct ids.
		for (i, spec) in KNOWN_EXTENSIONS.iter().enumerate() {
			assert!(!KNOWN_EXTENSIONS[i + 1..].iter().any(|later| later.id == spec.id), "extension number {} claimed twice", spec.id);
		}
	}

	#[test]
	fn fault_classes_are_the_three_c5_names() {
		assert_eq!(FrameFault::Integrity.fault_class(), "integrity");
		assert!(FrameFault::Integrity.is_integrity());
		assert_eq!(FrameFault::Undecodable { newer_format: true }.fault_class(), "newer_format");
		assert_eq!(FrameFault::Undecodable { newer_format: false }.fault_class(), "undecodable");
		assert!(!FrameFault::Undecodable { newer_format: false }.is_integrity());
	}
}
