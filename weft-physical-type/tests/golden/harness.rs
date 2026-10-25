//! The harness the FRE-3 golden suites share (`golden_frames.rs`, `golden_codecs.rs`).
//!
//! A golden is the exact byte string a writer produced on the day it was reviewed, committed as a
//! file, and compared against on every run. That is the whole difference from a round trip, and it
//! is the difference that matters for a storage format: a round trip passes whenever a writer and a
//! reader change together, because each is measured against the other rather than against the past.
//! A golden fails when the bytes move — the change that makes a file already written on a disk
//! unreadable, which [`crate::frame_registry`] promises cannot happen to a version that has
//! shipped ("A number that has shipped is never reused, redefined, or retired").
//!
//! # Regenerating a golden
//!
//! ```text
//! WEFT_UPDATE_GOLDENS=1 cargo test -p weft-physical-type --all-features --test golden_frames
//! ```
//!
//! rewrites the files instead of comparing them. Doing that *is* a format change: it needs either a
//! frame-version bump or a stated reason the bytes are allowed to move (a codec that only ever ran
//! behind an off-by-default feature, for instance), and the regenerated files need review as
//! fixtures — a diff that is only re-recorded is a golden that has stopped protecting anything.

// The two golden suites use different halves of this harness, so each compiles it with parts
// unused; that is the point of sharing it rather than duplicating it.
#![allow(dead_code)]
use std::path::PathBuf;

use weft_physical_type::crc32;

/// Setting this environment variable rewrites the goldens instead of comparing against them.
const UPDATE_VAR: &str = "WEFT_UPDATE_GOLDENS";

/// The directory the committed goldens live in: `weft-physical-type/tests/golden`.
#[must_use]
pub fn golden_dir() -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Compare `fresh` against the committed golden called `name`, byte for byte.
///
/// Under [`UPDATE_VAR`] the file is written instead. The panic on mismatch reports both lengths and
/// both CRC-32s plus the first offset at which they disagree, because the usual question at that
/// point is "did one byte of the header change, or did every offset shift?" — and a shifted suffix
/// means a field was added or a length changed, while a single byte means a tag or a codec choice
/// moved.
pub fn check_golden(name: &str, fresh: &[u8]) {
	let path = golden_dir().join(name);
	if std::env::var_os(UPDATE_VAR).is_some() {
		std::fs::write(&path, fresh).unwrap_or_else(|error| panic!("{}: cannot rewrite the golden: {error}", path.display()));
		return;
	}
	let committed = std::fs::read(&path).unwrap_or_else(|error| {
		panic!(
			"{name}: no committed golden at {} ({error}).\nCreate it with `{UPDATE_VAR}=1 cargo test -p weft-physical-type --all-features`, \
			 then review the new files as fixtures before committing them.",
			path.display()
		)
	});
	if committed != fresh {
		panic!("{}", mismatch_report(name, &committed, fresh));
	}
}

/// The body of the mismatch panic: lengths, checksums, and where the two parts first differ.
fn mismatch_report(name: &str, committed: &[u8], fresh: &[u8]) -> String {
	let first_diff = match committed.iter().zip(fresh).position(|(a, b)| a != b) {
		Some(index) => index,
		None => std::cmp::min(committed.len(), fresh.len()),
	};
	format!(
		"{name}: the bytes the writer produced are not the committed golden.\n\
		 committed: {} bytes, crc32 {:#010x}\n\
		 written:   {} bytes, crc32 {:#010x}\n\
		 first difference at offset {first_diff} ({first_diff:#04x})\n\
		 \
		 A frame whose bytes moved is a frame a released server can no longer read. Either restore \
		 the writer, or regenerate deliberately (`{UPDATE_VAR}=1 cargo test -p weft-physical-type \
		 --all-features`) and say in the commit why the layout change is allowed and which \
		 frame-version rule covers it.",
		committed.len(),
		crc32(committed),
		fresh.len(),
		crc32(fresh),
	)
}

/// `true` when the goldens are being rewritten rather than compared.
#[must_use]
pub fn updating() -> bool {
	std::env::var_os(UPDATE_VAR).is_some()
}

/// Compare a *rendered* golden — the text form of an encoding decision that has no byte stream of
/// its own (a run-length split, a delta-of-delta column, a codec selection) — against the committed
/// file for `name`.
///
/// Text rather than bytes because the failure a maintainer has to read is a semantic one: "the RLE
/// split now breaks this column into 41 runs instead of 12". Byte-level reporting would describe the
/// same event as an offset, and the whole point of pinning a decision rather than a frame is that the
/// decision is legible.
pub fn check_golden_text(name: &str, fresh: &str) {
	let path = golden_dir().join(name);
	if updating() {
		std::fs::write(&path, fresh).unwrap_or_else(|error| panic!("{}: cannot rewrite the golden: {error}", path.display()));
		return;
	}
	let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{name}: no committed text golden at {} ({error}).\nCreate it with `{UPDATE_VAR}=1 cargo test -p weft-physical-type --all-features`.", path.display()));
	if committed == fresh {
		return;
	}
	let committed_line = committed.lines().zip(fresh.lines()).position(|(a, b)| a != b);
	let (where_, left, right) = match committed_line {
		Some(index) => (format!("line {}", index + 1), committed.lines().nth(index).unwrap_or_default().to_string(), fresh.lines().nth(index).unwrap_or_default().to_string()),
		None => {
			// Every line matches and the lengths differ: one rendering emitted more rows than the other.
			(format!("line {}", std::cmp::min(committed.lines().count(), fresh.lines().count()) + 1), format!("<the committed golden has no line here; it runs to {} lines>", committed.lines().count()), format!("<the fresh rendering has no line here; it runs to {} lines>", fresh.lines().count()))
		}
	};
	panic!("{name}: the encoding decision this golden pins has changed.\n\
		 first difference at {where_}:\n\
		 committed: {left}\n\
		 written:   {right}\n\
		 \
		 A codec whose split, width or selection moved writes different bytes from today even where \
		 no frame fixture notices, because the frame writer picks one codec per column and a codec \
		 it does not pick is invisible there. Read the diff, then re-record deliberately with \
		 `{UPDATE_VAR}=1` and say what changed in the commit message.",)
}
