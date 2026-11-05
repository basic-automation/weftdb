//! Memory-mapped frame reads (roadmap Phase 5, "Memory-mapped GPU I/O").
//!
//! A range read ([`SegmentStore::read_time_range_of`](super::SegmentStore::read_time_range_of),
//! which the interpolation read path and so the GPU path use) normally reads each overlapping
//! `.weftseg` file into a fresh heap buffer and decodes the window from it. With
//! [`SegmentStoreOptions::mmap_reads`](super::SegmentStoreOptions::mmap_reads) on, in a build
//! with the `mmap-reads` cargo feature, it maps the file instead and decodes straight from the
//! page cache: the file-sized copy into the heap (and its allocation) is gone. The decoded rows
//! are the same bytes decoded by the same code, so the answer is identical
//! (`mapped_read_tests`).
//!
//! What this does **not** remove: a frame's columns are compressed, so the knots splimes gets
//! are still decoded (and, on the interpolation path, lifted to `f64`) into their own buffers,
//! and splimes uploads those. A mapped file cannot be handed to the GPU as a column: no stored
//! column is laid out as raw `f64`/`i64` knots.
//!
//! # Safety argument
//!
//! Mapping a file is `unsafe` because the mapped bytes must not change while a `&[u8]` over them
//! is alive, and a mapped page whose file was truncated faults (`SIGBUS`) when touched. For a
//! sealed frame:
//!
//! - WeftDB never writes into or truncates a sealed frame. A frame is written once under a
//!   fresh name (`create_new`), synced, then published by rename; maintenance writes new frames
//!   and retires old ones; quarantine and fsck move files by rename. None of them opens a
//!   published frame for writing.
//! - A retired frame is unlinked only after every read pinned before its retirement has
//!   finished (the reaper's epoch pin, which the range read holds for its whole duration, and
//!   the mapping is dropped before the read returns). On the Unix targets WeftDB ships, an
//!   unlink does not invalidate an existing mapping in any case.
//!
//! The residual risk is outside WeftDB: another process (an operator's tool, a broken backup
//! restore) truncating or rewriting a frame in place while a read has it mapped. The buffered
//! read would return an error or stale bytes there; a mapped read can take the process down with
//! `SIGBUS`. That is why the mapping is opt-in (off by default, and compiled out without the
//! feature) rather than the default read.

use std::{ops::Deref, path::Path};

/// A frame's bytes: read into the heap, or (with the `mmap-reads` feature) mapped.
pub(crate) enum FrameBytes {
	/// The buffered read's bytes.
	Owned(Vec<u8>),
	/// A read-only mapping of the frame file.
	#[cfg(feature = "mmap-reads")]
	Mapped(memmap2::Mmap),
}

impl Deref for FrameBytes {
	type Target = [u8];

	fn deref(&self) -> &[u8] {
		match self {
			Self::Owned(bytes) => bytes,
			#[cfg(feature = "mmap-reads")]
			Self::Mapped(map) => map,
		}
	}
}

/// Whether `WEFT_SEGMENT_MMAP_READS`'s value turns mapped reads on: `1`, `true`, `on` or
/// `yes` in any case; anything else, or unset, is off (the server's strict configuration
/// check refuses a value that is neither on nor off; this leniency only reaches embedders).
pub(crate) fn mmap_reads_from(value: Option<&str>) -> bool {
	let on = value.map(str::trim).is_some_and(|v| ["1", "true", "on", "yes"].iter().any(|word| v.eq_ignore_ascii_case(word)));
	if on && !MMAP_COMPILED {
		static WARNED: std::sync::Once = std::sync::Once::new();
		WARNED.call_once(|| tracing::warn!("WEFT_SEGMENT_MMAP_READS is on, but this build does not include memory-mapped reads (cargo feature `mmap-reads`); range reads stay buffered"));
	}
	on
}

/// Map the frame at `path` read-only. An empty file is returned as an empty owned buffer
/// (there is nothing to map).
///
/// # Errors
///
/// Opening, stat-ing or mapping the file fails.
#[cfg(feature = "mmap-reads")]
pub(crate) fn map_frame(path: &Path) -> std::io::Result<FrameBytes> {
	let file = std::fs::File::open(path)?;
	if file.metadata()?.len() == 0 {
		return Ok(FrameBytes::Owned(Vec::new()));
	}
	// SAFETY: `path` is a sealed `.weftseg` frame under this store's `segments/`, which WeftDB
	// never writes into or truncates once published, and which is not unlinked while the
	// caller's read pin is held (the mapping lives inside that read). See the module docs for
	// the full argument and the one residual risk (an outside process truncating the file),
	// which is why this path is opt-in.
	let map = unsafe { memmap2::Mmap::map(&file)? };
	Ok(FrameBytes::Mapped(map))
}

/// Without the `mmap-reads` feature there is no mapping: the frame is read into the heap,
/// exactly as the buffered path reads it.
///
/// # Errors
///
/// Reading the file fails.
#[cfg(not(feature = "mmap-reads"))]
pub(crate) fn map_frame(path: &Path) -> std::io::Result<FrameBytes> {
	std::fs::read(path).map(FrameBytes::Owned)
}

/// Whether this build can map frames at all (the `mmap-reads` feature).
pub(crate) const MMAP_COMPILED: bool = cfg!(feature = "mmap-reads");

#[cfg(test)]
mod tests {
	use super::mmap_reads_from;

	#[test]
	fn the_env_value_is_on_only_for_the_on_words() {
		for on in ["1", "true", "TRUE", " on ", "Yes"] {
			assert!(mmap_reads_from(Some(on)), "{on:?} turns mapped reads on");
		}
		for off in ["0", "false", "off", "no", "", "maybe"] {
			assert!(!mmap_reads_from(Some(off)), "{off:?} leaves mapped reads off");
		}
		assert!(!mmap_reads_from(None), "unset is off");
	}
}
