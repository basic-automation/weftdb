//! Aspect-name validation: the one check every aspect name passes before it can name a
//! file.
//!
//! A [`SegmentStore`](crate::SegmentStore) names each sealed frame, and its partial
//! sidecar, after the aspect: `segments/<aspect>-<id>.weftseg`. The aspect name is
//! therefore part of a filesystem path, and a name such as `../x` or `/tmp/x` would put
//! the frame outside the store root. [`validate`] rejects every name that could change
//! *where* that path points, or that Windows would open as something other than a plain
//! file in `segments/`:
//!
//! - an empty name, `.` and `..`;
//! - a leading `.`: it would make every frame of the aspect a hidden file, and every
//!   relative-path trick starts with one. No ordinary name needs it, so it is refused
//!   rather than special-cased;
//! - `/` or `\` anywhere (the Unix and Windows path separators);
//! - NUL or any other control character;
//! - a trailing `.` or space, which Windows strips (`a.` and `a` would name one file);
//! - a Windows reserved device name: `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`,
//!   `COM0`–`COM9`, `LPT0`–`LPT9` and the superscript `COM¹`–`COM³`/`LPT¹`–`LPT³`, in any
//!   case, and also followed by an extension or a stream suffix (`con.x`, `nul:x`, `CON
//!   .x`), which Windows opens as a device instead of a file. This rule applies on every
//!   platform, so a store keeps the same valid names wherever it runs;
//! - on Windows only, any of `<`, `>`, `:`, `"`, `|`, `?` and `*`, which Windows does not
//!   allow in a file name. A `:` would otherwise name an alternate data stream (`a:b`
//!   writes stream `b` of file `a`) or, after a single letter, a drive (`x:rate`), so
//!   such a name could be declared but never sealed;
//! - more than [`MAX_ASPECT_NAME_BYTES`] bytes, so a frame's file name stays well under the
//!   common 255-byte limit.
//!
//! Everything else is accepted, so names in use keep working: letters and digits in any
//! script, `-`, `_`, and `.` or spaces inside the name. On Unix `:` is accepted too
//! (`job:rate5m`): it is an ordinary file-name character there. Whatever the name, the
//! store also checks that every frame path it builds is a direct child of its
//! `segments/` directory.
//!
//! Names that differ only in letter case or in Unicode normalisation (`price` and
//! `PRICE`, or the NFC and NFD spellings of `é`) are distinct aspects but are not
//! rejected, although a case-insensitive filesystem (NTFS, APFS by default) or one that
//! normalises names (HFS+) stores their frames under one file name. That is a collision
//! between two aspects inside `segments/`, not a way out of it; the encoded frame names
//! of the crash-consistency design (`docs/design/crash-consistency.md` §4, slice S10)
//! are what remove it.

use std::fmt;

/// The longest aspect name [`validate`] accepts, in bytes of UTF-8.
///
/// A frame is named `<aspect>-<id>.weftseg`; with a 20-digit id that is at most 189
/// bytes, inside the 255-byte file-name limit of the common filesystems.
pub const MAX_ASPECT_NAME_BYTES: usize = 160;

/// The longest prefix of a rejected name an [`InvalidAspectName`] message quotes, in
/// characters, so an oversized name cannot flood a log line or an error response.
const MESSAGE_NAME_CHARS: usize = 64;

/// Windows device names that cannot be used as a file name, with or without an
/// extension or a stream suffix. Compared case-insensitively against the part of a name
/// before its first `.` or `:`, without trailing spaces.
const WINDOWS_RESERVED: [&str; 32] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³"];

/// Characters Windows does not allow in a file name, beyond the path separators and
/// control characters refused everywhere. Refused only when building for Windows.
const WINDOWS_FORBIDDEN: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// Why [`validate`] rejected an aspect name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AspectNameReason {
	/// The name is empty.
	Empty,
	/// The name is longer than [`MAX_ASPECT_NAME_BYTES`]; carries its length in bytes.
	TooLong(usize),
	/// The name is `.` or `..`.
	DotSegment,
	/// The name starts with `.`.
	LeadingDot,
	/// The name contains `/` or `\`.
	PathSeparator,
	/// The name contains NUL or another control character.
	ControlCharacter,
	/// The name ends with `.` or a space.
	TrailingDotOrSpace,
	/// The name is a Windows reserved device name.
	ReservedDeviceName,
	/// On Windows, the name contains a character Windows does not allow in a file name
	/// (`<`, `>`, `:`, `"`, `|`, `?` or `*`).
	WindowsForbiddenCharacter,
}

impl fmt::Display for AspectNameReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Empty => f.write_str("it is empty"),
			Self::TooLong(len) => write!(f, "it is {len} bytes long (the limit is {MAX_ASPECT_NAME_BYTES})"),
			Self::DotSegment => f.write_str("`.` and `..` are not names"),
			Self::LeadingDot => f.write_str("it starts with `.`"),
			Self::PathSeparator => f.write_str("it contains `/` or `\\`"),
			Self::ControlCharacter => f.write_str("it contains NUL or another control character"),
			Self::TrailingDotOrSpace => f.write_str("it ends with `.` or a space"),
			Self::ReservedDeviceName => f.write_str("it is a Windows reserved device name (CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM0-9, LPT0-9)"),
			Self::WindowsForbiddenCharacter => f.write_str("it contains a character Windows does not allow in a file name (< > : \" | ? *)"),
		}
	}
}

/// An aspect name [`validate`] rejected: the name and the [`AspectNameReason`].
///
/// Every [`SegmentStore`](crate::SegmentStore) call that would turn such a name into a
/// path returns this error (wrapped in an [`anyhow::Error`], from which it can be
/// recovered with `downcast_ref`), so a caller can tell a bad name from an I/O failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAspectName {
	/// The rejected name, in full.
	name: String,
	/// Why it was rejected.
	reason: AspectNameReason,
}

impl InvalidAspectName {
	/// The rejected name, in full.
	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Why the name was rejected.
	#[must_use]
	pub const fn reason(&self) -> AspectNameReason {
		self.reason
	}
}

impl fmt::Display for InvalidAspectName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let shown: String = self.name.chars().take(MESSAGE_NAME_CHARS).collect();
		let ellipsis = if self.name.chars().nth(MESSAGE_NAME_CHARS).is_some() { "…" } else { "" };
		write!(f, "invalid aspect name {shown:?}{ellipsis}: {}", self.reason)
	}
}

impl std::error::Error for InvalidAspectName {}

/// Check that `name` is safe to use as an aspect name, which also names the aspect's
/// files on disk. See the [module documentation](self) for the rules.
///
/// # Errors
///
/// An [`InvalidAspectName`] naming the first rule `name` breaks.
pub fn validate(name: &str) -> Result<(), InvalidAspectName> {
	reason(name, cfg!(windows)).map_or(Ok(()), |reason| Err(InvalidAspectName { name: name.to_string(), reason }))
}

/// The first rule `name` breaks, or [`None`] when it is a valid aspect name. `windows`
/// adds the Windows-only character rule; it is a parameter so both rule sets are tested
/// on every platform.
fn reason(name: &str, windows: bool) -> Option<AspectNameReason> {
	if name.is_empty() {
		return Some(AspectNameReason::Empty);
	}
	if name.len() > MAX_ASPECT_NAME_BYTES {
		return Some(AspectNameReason::TooLong(name.len()));
	}
	if name == "." || name == ".." {
		return Some(AspectNameReason::DotSegment);
	}
	if name.starts_with('.') {
		return Some(AspectNameReason::LeadingDot);
	}
	if name.contains(['/', '\\']) {
		return Some(AspectNameReason::PathSeparator);
	}
	if name.chars().any(char::is_control) {
		return Some(AspectNameReason::ControlCharacter);
	}
	if name.ends_with(['.', ' ']) {
		return Some(AspectNameReason::TrailingDotOrSpace);
	}
	// Windows reserves the device name with any extension (`CON.txt`) or stream suffix
	// (`NUL:x`), and ignores trailing spaces before either (`CON .txt`, `CON :x`).
	let stem = name.split(['.', ':']).next().unwrap_or(name).trim_end_matches(' ');
	if WINDOWS_RESERVED.iter().any(|reserved| reserved.eq_ignore_ascii_case(stem)) {
		return Some(AspectNameReason::ReservedDeviceName);
	}
	if windows && name.contains(WINDOWS_FORBIDDEN) {
		return Some(AspectNameReason::WindowsForbiddenCharacter);
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rejected(name: &str) -> AspectNameReason {
		validate(name).expect_err(name).reason()
	}

	#[test]
	fn ordinary_names_are_accepted() {
		for name in ["a", "price", "temp_1", "cpu.usage_idle", "load-avg", "a b", "Δt", "温度", "COM10", "LPT", "console", "nullable", "auxiliary", "con-x", "a..b", "conin", "CONOUT"] {
			assert_eq!(validate(name), Ok(()), "{name:?} should be accepted");
		}
		assert_eq!(reason("job:rate5m", false), None, "`:` is an ordinary character on Unix");
		assert_eq!(validate(&"x".repeat(MAX_ASPECT_NAME_BYTES)), Ok(()), "a name at the limit is accepted");
	}

	#[test]
	fn names_that_move_a_path_are_rejected() {
		assert_eq!(rejected(""), AspectNameReason::Empty);
		assert_eq!(rejected("."), AspectNameReason::DotSegment);
		assert_eq!(rejected(".."), AspectNameReason::DotSegment);
		assert_eq!(rejected(".hidden"), AspectNameReason::LeadingDot);
		assert_eq!(rejected("../x"), AspectNameReason::LeadingDot);
		assert_eq!(rejected("a/../../x"), AspectNameReason::PathSeparator);
		assert_eq!(rejected("/tmp/x"), AspectNameReason::PathSeparator);
		assert_eq!(rejected("a/b"), AspectNameReason::PathSeparator);
		assert_eq!(rejected("a\\b"), AspectNameReason::PathSeparator);
		assert_eq!(rejected("C:\\x"), AspectNameReason::PathSeparator);
	}

	#[test]
	fn control_characters_are_rejected() {
		for name in ["a\0b", "a\nb", "a\rb", "a\tb", "a\u{7f}", "a\u{85}b", "\u{1b}[31m"] {
			assert_eq!(rejected(name), AspectNameReason::ControlCharacter, "{name:?}");
		}
	}

	#[test]
	fn names_windows_would_alias_are_rejected() {
		assert_eq!(rejected("a."), AspectNameReason::TrailingDotOrSpace);
		assert_eq!(rejected("a "), AspectNameReason::TrailingDotOrSpace);
		for name in ["CON", "con", "Prn", "aux", "NUL", "nul.txt", "COM1", "com9", "COM0", "LPT1", "lpt9", "COM¹", "LPT³", "CON .x", "aux.price", "conin$", "CONOUT$", "CONIN$.x"] {
			assert_eq!(rejected(name), AspectNameReason::ReservedDeviceName, "{name:?}");
		}
	}

	/// Regression: Windows also ends a device stem at `:` (`nul:a` opens the NUL device),
	/// so a stream suffix must not hide a device name. Refused on every platform.
	#[test]
	fn device_names_with_a_stream_suffix_are_rejected() {
		for name in ["nul:x", "NUL:a", "COM1:x", "con:rate", "CON :x", "lpt1:a", "aux:", "conout$:x"] {
			for windows in [false, true] {
				assert_eq!(reason(name, windows), Some(AspectNameReason::ReservedDeviceName), "{name:?} (windows = {windows})");
			}
		}
		assert_eq!(validate("nul:x").unwrap_err().reason(), AspectNameReason::ReservedDeviceName);
	}

	/// On Windows a `:` names a stream or a drive and `<>"|?*` are not allowed in a file
	/// name, so such a name could be declared but never sealed; on Unix they are ordinary.
	#[test]
	fn windows_forbidden_characters_are_rejected_only_on_windows() {
		for name in ["job:rate5m", "x:rate", "a:b", "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b"] {
			assert_eq!(reason(name, true), Some(AspectNameReason::WindowsForbiddenCharacter), "{name:?} on Windows");
			assert_eq!(reason(name, false), None, "{name:?} on Unix");
		}
		assert_eq!(validate("job:rate5m").is_ok(), !cfg!(windows), "validate applies the rules of the platform it runs on");
	}

	#[test]
	fn overlong_names_are_rejected_and_the_message_is_bounded() {
		let long = "x".repeat(MAX_ASPECT_NAME_BYTES + 1);
		assert_eq!(rejected(&long), AspectNameReason::TooLong(MAX_ASPECT_NAME_BYTES + 1));
		// Multi-byte characters count in bytes, not characters.
		assert_eq!(rejected(&"é".repeat(81)), AspectNameReason::TooLong(162));
		let message = validate(&"y".repeat(100_000)).unwrap_err().to_string();
		assert!(message.len() < 200, "the message quotes a bounded prefix: {message}");
		assert!(message.contains('…'));
	}

	#[test]
	fn the_error_names_the_aspect_and_the_reason() {
		let err = validate("../x").unwrap_err();
		assert_eq!(err.name(), "../x");
		assert_eq!(err.to_string(), "invalid aspect name \"../x\": it starts with `.`");
	}
}
