//! Dictionary-name validation: the one check every dictionary name passes before it can
//! name a file.
//!
//! Each dictionary of an aspect is its own database, `<aspect>/dictionaries/<name>.db`, so
//! the name is part of a filesystem path. Unchecked, a name such as `../x` put the database
//! outside the aspect's `dictionaries/` directory, and an absolute one such as `/tmp/x`
//! replaced the whole path, where reading the dictionary's metadata created the file and
//! its tables. [`validate`] applies the rules of [`aspect_name`](crate::aspect_name), which
//! keep a name to one plain file name on every platform: not empty, `.` or `..`; no leading
//! `.`; no `/` or `\`; no NUL or other control character, and no bidirectional control or
//! invisible formatting character; no trailing `.` or space; no Windows device name (`CON`,
//! `NUL`, `COM1`, …); on Windows, none of `<>:"|?*`; and at most
//! [`MAX_DICTIONARY_NAME_BYTES`] bytes, so `<name>.db-log` stays inside the common 255-byte
//! file-name limit.
//!
//! The dictionary path builders
//! ([`Config::aspect_dictionaries_db_path`](crate::Config::aspect_dictionaries_db_path) and
//! [`Config::dictionary_path`](crate::Config::dictionary_path)) check the name first, so
//! every dictionary operation refuses an invalid name with an [`InvalidDictionaryName`]
//! error instead of building a path from it.

use std::fmt;

pub use crate::aspect_name::AspectNameReason as DictionaryNameReason;
use crate::aspect_name::{self, MESSAGE_NAME_CHARS};

/// The longest dictionary name [`validate`] accepts, in bytes of UTF-8: the aspect-name
/// limit, [`MAX_ASPECT_NAME_BYTES`](crate::aspect_name::MAX_ASPECT_NAME_BYTES).
pub const MAX_DICTIONARY_NAME_BYTES: usize = aspect_name::MAX_ASPECT_NAME_BYTES;

/// A dictionary name [`validate`] rejected: the name and the [`DictionaryNameReason`].
///
/// Every dictionary operation that would turn such a name into a path returns this error,
/// wrapped in an [`anyhow::Error`] from which it can be recovered with `downcast_ref`, so a
/// caller can tell a bad name from an I/O or database failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDictionaryName {
	/// The rejected name, in full.
	name: String,
	/// Why it was rejected.
	reason: DictionaryNameReason,
}

impl InvalidDictionaryName {
	/// The rejected name, in full.
	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Why the name was rejected.
	#[must_use]
	pub const fn reason(&self) -> DictionaryNameReason {
		self.reason
	}
}

impl fmt::Display for InvalidDictionaryName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let shown: String = self.name.chars().take(MESSAGE_NAME_CHARS).collect();
		let ellipsis = if self.name.chars().nth(MESSAGE_NAME_CHARS).is_some() { "…" } else { "" };
		write!(f, "invalid dictionary name {shown:?}{ellipsis}: {}", self.reason)
	}
}

impl std::error::Error for InvalidDictionaryName {}

/// Check that `name` is safe to use as a dictionary name, which also names the
/// dictionary's database file. See the [module documentation](self) for the rules.
///
/// # Errors
///
/// An [`InvalidDictionaryName`] naming the first rule `name` breaks.
pub fn validate(name: &str) -> Result<(), InvalidDictionaryName> {
	aspect_name::reason(name, cfg!(windows)).map_or(Ok(()), |reason| Err(InvalidDictionaryName { name: name.to_string(), reason }))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rejected(name: &str) -> DictionaryNameReason {
		validate(name).expect_err(name).reason()
	}

	#[test]
	fn ordinary_names_are_accepted() {
		for name in ["default", "pipeline", "Peak Detection Test", "BTCUSD Peaks", "a.b", "temp_1", "温度", "a..b"] {
			assert_eq!(validate(name), Ok(()), "{name:?} should be accepted");
		}
		assert_eq!(validate(&"x".repeat(MAX_DICTIONARY_NAME_BYTES)), Ok(()), "a name at the limit is accepted");
	}

	#[test]
	fn names_that_leave_the_dictionaries_folder_are_rejected() {
		assert_eq!(rejected(""), DictionaryNameReason::Empty);
		assert_eq!(rejected(".."), DictionaryNameReason::DotSegment);
		assert_eq!(rejected("../x"), DictionaryNameReason::LeadingDot);
		assert_eq!(rejected("a/../../x"), DictionaryNameReason::PathSeparator);
		assert_eq!(rejected("/tmp/x"), DictionaryNameReason::PathSeparator);
		assert_eq!(rejected("C:\\x"), DictionaryNameReason::PathSeparator);
		assert_eq!(rejected("a\0b"), DictionaryNameReason::ControlCharacter);
		assert_eq!(rejected("a\u{202E}b"), DictionaryNameReason::DeceptiveFormatCharacter);
		assert_eq!(rejected("a."), DictionaryNameReason::TrailingDotOrSpace);
		assert_eq!(rejected("NUL"), DictionaryNameReason::ReservedDeviceName);
		assert_eq!(rejected("con.backup"), DictionaryNameReason::ReservedDeviceName);
		assert_eq!(rejected(&"x".repeat(MAX_DICTIONARY_NAME_BYTES + 1)), DictionaryNameReason::TooLong(MAX_DICTIONARY_NAME_BYTES + 1));
	}

	#[test]
	fn the_error_names_the_dictionary_and_the_reason() {
		let err = validate("../x").unwrap_err();
		assert_eq!(err.name(), "../x");
		assert_eq!(err.to_string(), "invalid dictionary name \"../x\": it starts with `.`");
		let message = validate(&"y".repeat(100_000)).unwrap_err().to_string();
		assert!(message.len() < 200 && message.contains('…'), "the message quotes a bounded prefix: {message}");
	}
}
