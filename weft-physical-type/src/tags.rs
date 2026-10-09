//! Series tags (B-tags, TAG-1): the validated tag set that names a series, its canonical
//! byte form, and the selector a query picks series with.
//!
//! A series is an aspect plus a set of string tags such as `host=a, region=eu`. Three types
//! carry tags through every layer:
//!
//! - [`TagSet`]: a validated set of tags, the form ingest and reads take them in.
//! - [`SeriesKey`]: the canonical bytes of a tag set. The same bytes go into a frame (the
//!   `.weftseg` `SERIES_KEY` extension) and into the control plane's `series.series_key`,
//!   so checking that a frame belongs to a series is a byte compare.
//! - [`SeriesSelector`]: equality matchers, joined by AND, that pick the series a query
//!   reads.
//!
//! # Frozen format
//!
//! The rules and the canonical form below are **frozen format** from 1.0. The bytes they
//! produce are stored in frames and in the control plane, so changing a rule would make
//! stored series unreadable, or split one series in two. The caps and separators are the
//! public constants of this module.
//!
//! **Keys** ([`MAX_TAG_KEY_BYTES`]):
//! - 1 to 128 bytes;
//! - the first byte is an ASCII letter or `_`; every later byte is an ASCII letter, an
//!   ASCII digit, `_`, `.` or `-`;
//! - the prefix `__` ([`RESERVED_TAG_KEY_PREFIX`]) is reserved for system dimensions (see
//!   [Reserved keys](#reserved-keys)).
//!
//! **Values** ([`MAX_TAG_VALUE_BYTES`]):
//! - 1 to 256 bytes of UTF-8;
//! - no C0 control character (U+0000 to U+001F) and no U+007F. Every other character is
//!   allowed, C1 controls included;
//! - no Unicode normalization: values compare byte for byte, so the NFC and NFD spellings
//!   of `é` are two different values.
//!
//! **Tag sets** ([`MAX_TAGS`]):
//! - at most 16 tags, and no key twice. An empty value or a repeated key is an error, never
//!   silently dropped or merged;
//! - the empty set is a tag set too: it names series 0, the series of every untagged write.
//!
//! **Canonical key** ([`MAX_SERIES_KEY_BYTES`]):
//! - the tags sorted bytewise by key, each written as the key, the byte 0x1F
//!   ([`TAG_KEY_VALUE_SEPARATOR`]) and the value, joined by the byte 0x1E
//!   ([`TAG_PAIR_SEPARATOR`]). The empty set is the empty byte string;
//! - at most 1,024 bytes.
//!
//! Neither separator can occur in a valid key or value, so the canonical key needs no
//! escaping and is injective: reading it back splits at every 0x1E and then at the first
//! 0x1F of each pair, which recovers the tags exactly. It never depends on a serializer.
//! The canonical bytes are always valid UTF-8 (keys are ASCII, values are UTF-8, the
//! separators are ASCII), so they can also be stored as text ([`SeriesKey::as_str`]).
//!
//! Both separators sort below every byte a key or a value can hold. Ordering canonical
//! keys bytewise therefore orders tag sets as their sorted `(key, value)` lists compare,
//! and that is the [`Ord`] of [`TagSet`] and [`SeriesKey`].
//!
//! # Reserved keys
//!
//! A key that starts with `__` is reserved for system dimensions that a later WeftDB may
//! add. Input never creates one: [`TagSet::from_pairs`] and [`SeriesSelector::new`]
//! reject it with [`TagError::ReservedKey`].
//!
//! [`SeriesKey::from_canonical`] accepts reserved keys. Its bytes come from a frame or from
//! the control plane, which only WeftDB writes. A frame that a later version wrote with a
//! system dimension must stay readable by this version, rather than fail for carrying a
//! reserved key. Every other rule still applies on that path.
//!
//! A [`TagSet`] converted from such a key carries the reserved key, and a selector treats
//! it as it treats any tag it does not name. No selector can name a reserved key, so an
//! exact selector never matches such a set. A write path that accepts a [`TagSet`] built
//! from canonical bytes, rather than through [`TagSet::from_pairs`], should refuse one
//! whose keys start with [`RESERVED_TAG_KEY_PREFIX`].
//!
//! # Selecting series
//!
//! A [`SeriesSelector`] holds equality matchers, joined by AND, at most one per key:
//! - `(key, Some(value))` holds when the tag set has `key` with exactly that value;
//! - `(key, None)` holds when the tag set has no tag `key` (the HTTP spelling is `key=""`).
//!
//! A tag set matches when every matcher holds, and, for an *exact* selector, when it also
//! has no tag beyond the matchers that carry a value, so that it equals them. A selector
//! with no matchers selects every series; with no matchers and exact, it selects series 0
//! only. 1.0 has no `!=`, regex or OR matcher.
//!
//! # Example
//!
//! ```
//! use weft_physical_type::tags::{SeriesKey, SeriesSelector, TagSet};
//!
//! let tags = TagSet::from_pairs([("region", "eu"), ("host", "a")])?;
//! assert_eq!(tags.series_key().as_bytes(), b"host\x1fa\x1eregion\x1feu");
//!
//! // The canonical bytes, as read back from a frame, give the same tag set.
//! let stored = SeriesKey::from_canonical(b"host\x1fa\x1eregion\x1feu")?;
//! assert_eq!(TagSet::from(stored), tags);
//!
//! // `host` is `a` and there is no `rack` tag.
//! let selector = SeriesSelector::new([("host", Some("a")), ("rack", None)], false)?;
//! assert!(selector.matches(&tags));
//! # Ok::<(), weft_physical_type::tags::TagError>(())
//! ```

use std::{cmp::Ordering, fmt, iter::FusedIterator};

/// The longest tag key, in bytes. Frozen format.
pub const MAX_TAG_KEY_BYTES: usize = 128;

/// The longest tag value, in bytes of UTF-8. Frozen format.
pub const MAX_TAG_VALUE_BYTES: usize = 256;

/// The most tags one tag set holds. Frozen format.
pub const MAX_TAGS: usize = 16;

/// The longest canonical series key, in bytes. Frozen format.
pub const MAX_SERIES_KEY_BYTES: usize = 1024;

/// The byte between a key and its value in a canonical series key: 0x1F, the ASCII unit
/// separator. Frozen format.
pub const TAG_KEY_VALUE_SEPARATOR: u8 = 0x1F;

/// The byte between two tags in a canonical series key: 0x1E, the ASCII record separator.
/// Frozen format.
pub const TAG_PAIR_SEPARATOR: u8 = 0x1E;

/// The key prefix reserved for system dimensions. [`TagSet::from_pairs`] and
/// [`SeriesSelector::new`] reject a key that starts with it; see the
/// [module documentation](crate::tags#reserved-keys).
pub const RESERVED_TAG_KEY_PREFIX: &str = "__";

/// [`TAG_KEY_VALUE_SEPARATOR`] as a `char`, for building and splitting the canonical text.
const KEY_VALUE_SEPARATOR_CHAR: char = '\u{1f}';

/// [`TAG_PAIR_SEPARATOR`] as a `char`, for building and splitting the canonical text.
const PAIR_SEPARATOR_CHAR: char = '\u{1e}';

/// A validated set of tags: the identity of a series within its aspect.
///
/// Build one from `(key, value)` pairs with [`from_pairs`](Self::from_pairs), which applies
/// every rule of the [module documentation](crate::tags), or from stored canonical bytes
/// with [`SeriesKey::from_canonical`] and then `TagSet::from`. [`TagSet::new`] is the empty
/// set, series 0.
///
/// A tag set holds its [`SeriesKey`], so [`series_key`](Self::series_key) costs nothing,
/// and two tag sets are equal exactly when their canonical keys are. Its [`Ord`] is the
/// bytewise order of the canonical keys, which is also the order of the sorted
/// `(key, value)` lists.
///
/// ```
/// use weft_physical_type::tags::{TagError, TagSet};
///
/// let tags = TagSet::from_pairs([("host", "a"), ("dc", "x")])?;
/// assert_eq!(tags.get("host"), Some("a"));
/// assert_eq!(tags.iter().collect::<Vec<_>>(), [("dc", "x"), ("host", "a")]);
///
/// // A repeated key is an error, never a silent overwrite.
/// let repeated = TagSet::from_pairs([("host", "a"), ("host", "b")]);
/// assert_eq!(repeated, Err(TagError::DuplicateKey { key: "host".to_owned() }));
/// # Ok::<(), TagError>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagSet {
	/// The canonical form; the pairs are read from it on demand.
	key: SeriesKey,
}

impl TagSet {
	/// The empty tag set, which names series 0.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Build a tag set from `(key, value)` pairs given in any order.
	///
	/// The pairs are sorted by key into the canonical form, so the same tags in any order
	/// give the same tag set. No pairs give the empty set.
	///
	/// # Errors
	///
	/// [`TagError`] when the pairs break a rule of the
	/// [module documentation](crate::tags). The pairs are checked in input order, each key
	/// before its value; then the number of tags; then repeated keys; then the length of
	/// the canonical key. The first problem found is returned:
	/// - [`EmptyKey`](TagError::EmptyKey), [`KeyTooLong`](TagError::KeyTooLong),
	///   [`InvalidKey`](TagError::InvalidKey) or [`ReservedKey`](TagError::ReservedKey) for
	///   a key;
	/// - [`EmptyValue`](TagError::EmptyValue), [`ValueTooLong`](TagError::ValueTooLong) or
	///   [`ControlCharacter`](TagError::ControlCharacter) for a value;
	/// - [`TooManyTags`](TagError::TooManyTags), [`DuplicateKey`](TagError::DuplicateKey)
	///   or [`SeriesKeyTooLong`](TagError::SeriesKeyTooLong) for the set.
	pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self, TagError>
	where
		I: IntoIterator<Item = (K, V)>,
		K: AsRef<str>,
		V: AsRef<str>,
	{
		let mut held: Vec<(K, V)> = Vec::new();
		let mut count = 0_usize;
		for (key, value) in pairs {
			check_key(key.as_ref().as_bytes(), ReservedKeys::Reject)?;
			check_value(key.as_ref().as_bytes(), value.as_ref().as_bytes())?;
			count += 1;
			if count <= MAX_TAGS {
				held.push((key, value));
			}
		}
		if count > MAX_TAGS {
			return Err(TagError::TooManyTags { count });
		}
		held.sort_unstable_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
		let repeated = held.windows(2).find_map(|pair| match pair {
			[first, second] if first.0.as_ref() == second.0.as_ref() => Some(first.0.as_ref()),
			_ => None,
		});
		if let Some(key) = repeated {
			return Err(TagError::DuplicateKey { key: key_for_error(key.as_bytes()) });
		}
		let len = held.iter().map(|(key, value)| key.as_ref().len() + 1 + value.as_ref().len()).sum::<usize>() + held.len().saturating_sub(1);
		if len > MAX_SERIES_KEY_BYTES {
			return Err(TagError::SeriesKeyTooLong { len });
		}
		let mut canonical = String::with_capacity(len);
		for (key, value) in &held {
			push_pair(&mut canonical, key.as_ref(), value.as_ref());
		}
		Ok(Self { key: SeriesKey { canonical: canonical.into_boxed_str() } })
	}

	/// The value of tag `key`, or [`None`] when the set has no such tag.
	#[must_use]
	pub fn get(&self, key: &str) -> Option<&str> {
		for (candidate, value) in self {
			match candidate.cmp(key) {
				Ordering::Less => {}
				Ordering::Equal => return Some(value),
				Ordering::Greater => return None,
			}
		}
		None
	}

	/// Whether this is the empty set, series 0.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.key.is_empty()
	}

	/// The tags as `(key, value)` pairs, in ascending bytewise order of key.
	#[must_use]
	pub fn iter(&self) -> Iter<'_> {
		Iter { rest: self.key.as_str() }
	}

	/// The number of tags.
	#[must_use]
	pub fn len(&self) -> usize {
		if self.is_empty() {
			0
		} else {
			self.key.as_str().matches(PAIR_SEPARATOR_CHAR).count() + 1
		}
	}

	/// The canonical key of this tag set: the bytes a frame and the control plane store.
	#[must_use]
	pub const fn series_key(&self) -> &SeriesKey {
		&self.key
	}
}

impl fmt::Debug for TagSet {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_map().entries(self.iter()).finish()
	}
}

impl From<SeriesKey> for TagSet {
	/// The tag set a canonical key encodes. Free: a tag set holds its canonical key.
	fn from(key: SeriesKey) -> Self {
		Self { key }
	}
}

impl<'a> IntoIterator for &'a TagSet {
	type IntoIter = Iter<'a>;
	type Item = (&'a str, &'a str);

	fn into_iter(self) -> Iter<'a> {
		self.iter()
	}
}

/// The tags of a [`TagSet`] as `(key, value)` pairs, in ascending bytewise order of key;
/// returned by [`TagSet::iter`].
#[derive(Clone, Debug)]
pub struct Iter<'a> {
	/// The canonical text not yet visited.
	rest: &'a str,
}

impl<'a> Iterator for Iter<'a> {
	type Item = (&'a str, &'a str);

	fn next(&mut self) -> Option<Self::Item> {
		if self.rest.is_empty() {
			return None;
		}
		let (pair, rest) = self.rest.split_once(PAIR_SEPARATOR_CHAR).unwrap_or((self.rest, ""));
		self.rest = rest;
		let split = pair.split_once(KEY_VALUE_SEPARATOR_CHAR);
		if split.is_none() {
			// Unreachable: every pair of a validated key holds the separator. Stop for good
			// rather than resume after a pair that could not be read.
			self.rest = "";
		}
		split
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		(usize::from(!self.rest.is_empty()), Some(MAX_TAGS))
	}
}

impl FusedIterator for Iter<'_> {}

/// The canonical bytes of a [`TagSet`]: the frozen form a frame's `SERIES_KEY` extension
/// and the control plane's `series.series_key` store.
///
/// A key is always valid: it comes from a [`TagSet`] (`SeriesKey::from` or
/// [`TagSet::series_key`]) or from [`from_canonical`](Self::from_canonical), which checks
/// every rule. The empty key, [`SeriesKey::default`], names series 0. Keys compare, order
/// and hash by their bytes.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesKey {
	/// The canonical bytes, which are always valid UTF-8.
	canonical: Box<str>,
}

impl SeriesKey {
	/// Read a canonical key back from stored bytes, as a frame or the control plane holds
	/// them.
	///
	/// This is the storage path: it accepts keys with the reserved `__` prefix, so that a
	/// frame a later version wrote with a system dimension stays readable (see the
	/// [module documentation](crate::tags#reserved-keys)). Every other rule is checked, so
	/// the result round-trips: `from_canonical(k.as_bytes())` gives `k` back for every key
	/// `k`.
	///
	/// # Errors
	///
	/// [`TagError`] when the bytes are not a canonical key. The length is checked first;
	/// then the pairs in order, each for its separator, its key, its value and its order
	/// after the previous key; then the number of tags. The first problem found is
	/// returned:
	/// - [`SeriesKeyTooLong`](TagError::SeriesKeyTooLong) beyond 1,024 bytes;
	/// - [`MissingSeparator`](TagError::MissingSeparator) for a pair with no 0x1F (an empty
	///   pair included, from a leading, trailing or doubled 0x1E);
	/// - [`EmptyKey`](TagError::EmptyKey), [`KeyTooLong`](TagError::KeyTooLong) or
	///   [`InvalidKey`](TagError::InvalidKey) for a key;
	/// - [`EmptyValue`](TagError::EmptyValue), [`ValueTooLong`](TagError::ValueTooLong),
	///   [`ControlCharacter`](TagError::ControlCharacter) or
	///   [`ValueNotUtf8`](TagError::ValueNotUtf8) for a value;
	/// - [`DuplicateKey`](TagError::DuplicateKey) or
	///   [`UnsortedKeys`](TagError::UnsortedKeys) for a key that does not sort strictly
	///   after the previous one;
	/// - [`TooManyTags`](TagError::TooManyTags) beyond 16 pairs.
	pub fn from_canonical(bytes: &[u8]) -> Result<Self, TagError> {
		if bytes.len() > MAX_SERIES_KEY_BYTES {
			return Err(TagError::SeriesKeyTooLong { len: bytes.len() });
		}
		if bytes.is_empty() {
			return Ok(Self::default());
		}
		let mut canonical = String::with_capacity(bytes.len());
		let mut previous: Option<&[u8]> = None;
		let mut count = 0_usize;
		for (index, pair) in bytes.split(|&byte| byte == TAG_PAIR_SEPARATOR).enumerate() {
			let Some(at) = pair.iter().position(|&byte| byte == TAG_KEY_VALUE_SEPARATOR) else {
				return Err(TagError::MissingSeparator { pair: index });
			};
			let (key, separator_and_value) = pair.split_at(at);
			let value = separator_and_value.get(1..).unwrap_or_default();
			let key_text = check_key(key, ReservedKeys::Accept)?;
			let value_text = check_value(key, value)?;
			if let Some(previous) = previous {
				match previous.cmp(key) {
					Ordering::Less => {}
					Ordering::Equal => return Err(TagError::DuplicateKey { key: key_for_error(key) }),
					Ordering::Greater => return Err(TagError::UnsortedKeys { key: key_for_error(key), previous: key_for_error(previous) }),
				}
			}
			previous = Some(key);
			count += 1;
			push_pair(&mut canonical, key_text, value_text);
		}
		if count > MAX_TAGS {
			return Err(TagError::TooManyTags { count });
		}
		debug_assert_eq!(canonical.as_bytes(), bytes, "a validated canonical key is rebuilt byte for byte");
		Ok(Self { canonical: canonical.into_boxed_str() })
	}

	/// The canonical bytes.
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		self.canonical.as_bytes()
	}

	/// The canonical bytes as text. They are always valid UTF-8; the separators are the
	/// control characters U+001E and U+001F.
	#[must_use]
	pub fn as_str(&self) -> &str {
		&self.canonical
	}

	/// Whether this is the empty key, series 0.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.canonical.is_empty()
	}

	/// The length of the canonical key, in bytes (at most [`MAX_SERIES_KEY_BYTES`]).
	#[must_use]
	pub fn len(&self) -> usize {
		self.canonical.len()
	}
}

impl fmt::Debug for SeriesKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_tuple("SeriesKey").field(&self.as_str()).finish()
	}
}

impl From<TagSet> for SeriesKey {
	/// The canonical key of a tag set. Free: a tag set holds its canonical key.
	fn from(tags: TagSet) -> Self {
		tags.key
	}
}

/// Picks the series a query reads: equality matchers, joined by AND, at most one per key.
///
/// See [Selecting series](crate::tags#selecting-series) for the rules.
/// [`all`](Self::all) (also [`Default`]) selects every series and
/// [`untagged`](Self::untagged) only series 0; [`new`](Self::new) builds any other
/// selector. Two selectors are equal when they hold the same matchers, in whatever order
/// they were given, and the same exactness.
///
/// ```
/// use weft_physical_type::tags::{SeriesSelector, TagSet};
///
/// let host_a = TagSet::from_pairs([("host", "a")])?;
/// let host_a_dc_x = TagSet::from_pairs([("host", "a"), ("dc", "x")])?;
///
/// let loose = SeriesSelector::new([("host", Some("a"))], false)?;
/// assert!(loose.matches(&host_a) && loose.matches(&host_a_dc_x));
///
/// // Exact: the tag set must be exactly `host=a`.
/// let exact = SeriesSelector::new([("host", Some("a"))], true)?;
/// assert!(exact.matches(&host_a) && !exact.matches(&host_a_dc_x));
///
/// // `dc` must be absent.
/// let no_dc = SeriesSelector::new([("dc", None::<&str>)], false)?;
/// assert!(no_dc.matches(&host_a) && !no_dc.matches(&host_a_dc_x));
///
/// assert!(SeriesSelector::untagged().matches(&TagSet::new()));
/// assert!(!SeriesSelector::untagged().matches(&host_a));
/// # Ok::<(), weft_physical_type::tags::TagError>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SeriesSelector {
	/// The matchers, sorted bytewise by key, each key once. `None` means "absent".
	matchers: Vec<(Box<str>, Option<Box<str>>)>,
	/// Whether a matching tag set must equal the matchers that carry a value.
	exact: bool,
}

impl SeriesSelector {
	/// The selector with no matchers, not exact: every series matches, series 0 included.
	#[must_use]
	pub const fn all() -> Self {
		Self { matchers: Vec::new(), exact: false }
	}

	/// The selector with no matchers, exact: only the empty tag set, series 0, matches.
	#[must_use]
	pub const fn untagged() -> Self {
		Self { matchers: Vec::new(), exact: true }
	}

	/// Build a selector from `(key, value)` matchers given in any order.
	///
	/// `(key, Some(value))` requires tag `key` with exactly `value`; `(key, None)` requires
	/// that the tag set has no tag `key`. With `exact`, a matching tag set must also have
	/// no tag beyond the matchers that carry a value.
	///
	/// # Errors
	///
	/// [`TagError`] for the first matcher, in input order, whose key or value breaks the
	/// rules a [`TagSet`] key or value follows, and then [`TagError::DuplicateKey`] when a
	/// key appears in more than one matcher. A key with the reserved `__` prefix is
	/// [`TagError::ReservedKey`]. `Some("")` is [`TagError::EmptyValue`]: absence is
	/// spelled `None`.
	pub fn new<I, K, V>(matchers: I, exact: bool) -> Result<Self, TagError>
	where
		I: IntoIterator<Item = (K, Option<V>)>,
		K: AsRef<str>,
		V: AsRef<str>,
	{
		let mut held: Vec<(Box<str>, Option<Box<str>>)> = Vec::new();
		for (key, value) in matchers {
			let key = key.as_ref();
			check_key(key.as_bytes(), ReservedKeys::Reject)?;
			let value = value.as_ref().map(|value| check_value(key.as_bytes(), value.as_ref().as_bytes())).transpose()?;
			held.push((key.into(), value.map(Into::into)));
		}
		held.sort_unstable_by(|a, b| a.0.cmp(&b.0));
		let repeated = held.windows(2).find_map(|pair| match pair {
			[first, second] if first.0 == second.0 => Some(&first.0),
			_ => None,
		});
		if let Some(key) = repeated {
			return Err(TagError::DuplicateKey { key: key_for_error(key.as_bytes()) });
		}
		Ok(Self { matchers: held, exact })
	}

	/// Whether a matching tag set must equal the matchers that carry a value.
	#[must_use]
	pub const fn is_exact(&self) -> bool {
		self.exact
	}

	/// Whether `tags` is selected: every matcher holds and, for an exact selector, `tags`
	/// has no tag beyond the matchers that carry a value.
	#[must_use]
	pub fn matches(&self, tags: &TagSet) -> bool {
		let mut valued = 0_usize;
		for (key, wanted) in &self.matchers {
			let found = tags.get(key);
			match wanted {
				Some(value) => {
					if found != Some(&**value) {
						return false;
					}
					valued += 1;
				}
				None => {
					if found.is_some() {
						return false;
					}
				}
			}
		}
		!self.exact || tags.len() == valued
	}

	/// The matchers as `(key, value)` pairs, in ascending bytewise order of key; a value of
	/// [`None`] means the tag must be absent.
	#[must_use]
	pub fn matchers(&self) -> Matchers<'_> {
		Matchers { inner: self.matchers.iter() }
	}
}

/// The matchers of a [`SeriesSelector`] as `(key, value)` pairs, in ascending bytewise
/// order of key; returned by [`SeriesSelector::matchers`].
#[derive(Clone, Debug)]
pub struct Matchers<'a> {
	/// The matchers not yet visited.
	inner: std::slice::Iter<'a, (Box<str>, Option<Box<str>>)>,
}

impl<'a> Iterator for Matchers<'a> {
	type Item = (&'a str, Option<&'a str>);

	fn next(&mut self) -> Option<Self::Item> {
		self.inner.next().map(|(key, value)| (&**key, value.as_deref()))
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		self.inner.size_hint()
	}
}

impl ExactSizeIterator for Matchers<'_> {}

impl FusedIterator for Matchers<'_> {}

/// Why tags, canonical bytes or selector matchers were rejected.
///
/// Every variant that concerns one tag names its key. A key quoted in an error is cut to
/// its first [`MAX_TAG_KEY_BYTES`] bytes (only a [`KeyTooLong`](Self::KeyTooLong) key is
/// longer), and a byte of it that is not UTF-8, possible only in canonical bytes, is
/// replaced with U+FFFD. The [`Display`](fmt::Display) form quotes keys escaped, so a
/// control character in one cannot break a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TagError {
	/// A tag key is empty.
	EmptyKey,
	/// A tag key is longer than [`MAX_TAG_KEY_BYTES`].
	KeyTooLong {
		/// The key, cut to its first [`MAX_TAG_KEY_BYTES`] bytes, never inside a character.
		key: String,
		/// The length of the key, in bytes.
		len: usize,
	},
	/// A tag key holds a byte the key grammar does not allow where it stands: the first
	/// byte must be an ASCII letter or `_`, and every later one an ASCII letter, an ASCII
	/// digit, `_`, `.` or `-`.
	InvalidKey {
		/// The key.
		key: String,
		/// The offset of the first byte not allowed, in bytes from the start of the key.
		index: usize,
	},
	/// A tag key starts with [`RESERVED_TAG_KEY_PREFIX`], which is reserved for system
	/// dimensions.
	ReservedKey {
		/// The key.
		key: String,
	},
	/// A tag value is empty. A selector spells an absent tag `None`, not `Some("")`.
	EmptyValue {
		/// The key of the tag.
		key: String,
	},
	/// A tag value is longer than [`MAX_TAG_VALUE_BYTES`].
	ValueTooLong {
		/// The key of the tag.
		key: String,
		/// The length of the value, in bytes.
		len: usize,
	},
	/// A tag value contains a C0 control character (U+0000 to U+001F) or U+007F.
	ControlCharacter {
		/// The key of the tag.
		key: String,
		/// The offset of the first such character, in bytes from the start of the value.
		index: usize,
		/// The character.
		character: char,
	},
	/// A tag value in canonical bytes is not valid UTF-8.
	ValueNotUtf8 {
		/// The key of the tag.
		key: String,
		/// The offset of the first byte that is not valid UTF-8, in bytes from the start
		/// of the value.
		index: usize,
	},
	/// A tag key appears more than once: twice in a tag set, twice in a selector, or
	/// twice in canonical bytes.
	DuplicateKey {
		/// The key.
		key: String,
	},
	/// More than [`MAX_TAGS`] tags.
	TooManyTags {
		/// The number of tags given.
		count: usize,
	},
	/// The canonical key is longer than [`MAX_SERIES_KEY_BYTES`].
	SeriesKeyTooLong {
		/// The length of the canonical key, in bytes.
		len: usize,
	},
	/// Canonical bytes list a key before a key that sorts below it: the pairs of a
	/// canonical key are in ascending bytewise order of key.
	UnsortedKeys {
		/// The key that is out of order.
		key: String,
		/// The key just before it, which sorts above it.
		previous: String,
	},
	/// A pair of canonical bytes has no key-value separator ([`TAG_KEY_VALUE_SEPARATOR`]).
	/// An empty pair, left by a leading, trailing or doubled [`TAG_PAIR_SEPARATOR`], has
	/// none either.
	MissingSeparator {
		/// The position of the pair, counting from 0.
		pair: usize,
	},
}

impl fmt::Display for TagError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::EmptyKey => f.write_str("a tag key is empty"),
			Self::KeyTooLong { key, len } => write!(f, "the tag key starting {key:?} is {len} bytes long; a key holds at most {MAX_TAG_KEY_BYTES} bytes"),
			Self::InvalidKey { key, index: 0 } => write!(f, "the tag key {key:?} does not start with an ASCII letter or `_`"),
			Self::InvalidKey { key, index } => write!(f, "the tag key {key:?} has a character other than an ASCII letter, an ASCII digit, `_`, `.` or `-` at byte {index}"),
			Self::ReservedKey { key } => write!(f, "the tag key {key:?} starts with `{RESERVED_TAG_KEY_PREFIX}`, which is reserved for system dimensions"),
			Self::EmptyValue { key } => write!(f, "the value of tag {key:?} is empty"),
			Self::ValueTooLong { key, len } => write!(f, "the value of tag {key:?} is {len} bytes long; a value holds at most {MAX_TAG_VALUE_BYTES} bytes"),
			Self::ControlCharacter { key, index, character } => write!(f, "the value of tag {key:?} contains the control character U+{:04X} at byte {index}; values may not contain U+0000 to U+001F or U+007F", u32::from(*character)),
			Self::ValueNotUtf8 { key, index } => write!(f, "the value of tag {key:?} is not valid UTF-8 from byte {index}"),
			Self::DuplicateKey { key } => write!(f, "the tag key {key:?} appears more than once"),
			Self::TooManyTags { count } => write!(f, "{count} tags given; a tag set holds at most {MAX_TAGS}"),
			Self::SeriesKeyTooLong { len } => write!(f, "the canonical series key is {len} bytes long; it may be at most {MAX_SERIES_KEY_BYTES} bytes"),
			Self::UnsortedKeys { key, previous } => write!(f, "the canonical series key lists the tag key {key:?} after {previous:?}; keys must be in ascending byte order"),
			Self::MissingSeparator { pair } => write!(f, "pair {pair} of the canonical series key has no key-value separator (0x1F)"),
		}
	}
}

impl std::error::Error for TagError {}

/// Whether a key check accepts the reserved `__` prefix: only the storage path does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReservedKeys {
	/// Input: [`TagSet::from_pairs`] and [`SeriesSelector::new`].
	Reject,
	/// Stored canonical bytes: [`SeriesKey::from_canonical`].
	Accept,
}

/// Whether `byte` may start a key: an ASCII letter or `_`.
const fn is_key_start(byte: u8) -> bool {
	byte.is_ascii_alphabetic() || byte == b'_'
}

/// Whether `byte` may follow the first byte of a key: an ASCII letter or digit, `_`, `.`
/// or `-`.
const fn is_key_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

/// Check `key` against the key rules, returning it as text.
fn check_key(key: &[u8], reserved: ReservedKeys) -> Result<&str, TagError> {
	let Some(&first) = key.first() else {
		return Err(TagError::EmptyKey);
	};
	if key.len() > MAX_TAG_KEY_BYTES {
		return Err(TagError::KeyTooLong { key: key_for_error(key), len: key.len() });
	}
	if !is_key_start(first) {
		return Err(TagError::InvalidKey { key: key_for_error(key), index: 0 });
	}
	if let Some(offset) = key.iter().skip(1).position(|&byte| !is_key_byte(byte)) {
		return Err(TagError::InvalidKey { key: key_for_error(key), index: offset + 1 });
	}
	if reserved == ReservedKeys::Reject && key.starts_with(RESERVED_TAG_KEY_PREFIX.as_bytes()) {
		return Err(TagError::ReservedKey { key: key_for_error(key) });
	}
	// Every byte is ASCII by now, so this checked conversion always succeeds.
	std::str::from_utf8(key).map_err(|error| TagError::InvalidKey { key: key_for_error(key), index: error.valid_up_to() })
}

/// Check `value`, the value of tag `key`, against the value rules, returning it as text.
fn check_value<'v>(key: &[u8], value: &'v [u8]) -> Result<&'v str, TagError> {
	if value.is_empty() {
		return Err(TagError::EmptyValue { key: key_for_error(key) });
	}
	if value.len() > MAX_TAG_VALUE_BYTES {
		return Err(TagError::ValueTooLong { key: key_for_error(key), len: value.len() });
	}
	// `is_ascii_control` is exactly U+0000 to U+001F and U+007F. Every byte of a multi-byte
	// UTF-8 character is 0x80 or above, so checking bytes checks characters.
	if let Some((index, &byte)) = value.iter().enumerate().find(|(_, byte)| byte.is_ascii_control()) {
		return Err(TagError::ControlCharacter { key: key_for_error(key), index, character: char::from(byte) });
	}
	std::str::from_utf8(value).map_err(|error| TagError::ValueNotUtf8 { key: key_for_error(key), index: error.valid_up_to() })
}

/// Append one pair to canonical text, with a pair separator first unless it is the first.
fn push_pair(canonical: &mut String, key: &str, value: &str) {
	if !canonical.is_empty() {
		canonical.push(PAIR_SEPARATOR_CHAR);
	}
	canonical.push_str(key);
	canonical.push(KEY_VALUE_SEPARATOR_CHAR);
	canonical.push_str(value);
}

/// A key as an error quotes it: at most its first [`MAX_TAG_KEY_BYTES`] bytes, never cut
/// inside a character of a UTF-8 key, with bytes that are not UTF-8 replaced by U+FFFD.
fn key_for_error(key: &[u8]) -> String {
	let mut end = key.len().min(MAX_TAG_KEY_BYTES);
	if let Ok(text) = std::str::from_utf8(key) {
		while !text.is_char_boundary(end) {
			end -= 1;
		}
	}
	String::from_utf8_lossy(&key[..end]).into_owned()
}

#[cfg(test)]
mod tests {
	use std::collections::{BTreeMap, BTreeSet, HashMap};

	use super::*;

	/// A seeded xorshift64 generator, so every property test is reproducible.
	struct Rng(u64);

	impl Rng {
		fn next(&mut self) -> u64 {
			let mut x = self.0;
			x ^= x << 13;
			x ^= x >> 7;
			x ^= x << 17;
			self.0 = x;
			x
		}

		/// A number in `0..n`.
		fn below(&mut self, n: usize) -> usize {
			let n = u64::try_from(n).expect("n fits u64");
			usize::try_from(self.next() % n).expect("below n")
		}

		fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
			&items[self.below(items.len())]
		}

		fn shuffle<T>(&mut self, items: &mut [T]) {
			for i in (1..items.len()).rev() {
				let j = self.below(i + 1);
				items.swap(i, j);
			}
		}
	}

	const KEY_START: &[u8] = b"ABCXYZabcxyz_";
	const KEY_REST: &[u8] = b"ABCXYZabcxyz0189_.-";
	/// Value characters: printable ASCII, the characters that matter to other encodings,
	/// multi-byte UTF-8 and a C1 control (allowed).
	const VALUE_CHARS: &[char] = &['a', 'z', 'A', '0', ' ', '=', ',', '"', '\\', '{', '}', '~', 'é', 'ß', '中', '😀', '\u{80}', '\u{85}', '\u{feff}', '\u{301}'];

	fn random_key(rng: &mut Rng, max_len: usize) -> String {
		let len = 1 + rng.below(max_len);
		let mut key = String::with_capacity(len);
		key.push(char::from(*rng.pick(KEY_START)));
		while key.len() < len {
			let byte = *rng.pick(KEY_REST);
			// Never form the reserved prefix.
			if key == "_" && byte == b'_' {
				continue;
			}
			key.push(char::from(byte));
		}
		key
	}

	fn random_value(rng: &mut Rng, max_len: usize) -> String {
		let target = 1 + rng.below(max_len);
		let mut value = String::new();
		while value.len() < target {
			let c = *rng.pick(VALUE_CHARS);
			if value.len() + c.len_utf8() > max_len {
				break;
			}
			value.push(c);
		}
		if value.is_empty() {
			value.push('v');
		}
		value
	}

	/// Random pairs with distinct keys; some sets are large enough to pass the canonical cap.
	fn random_pairs(rng: &mut Rng) -> Vec<(String, String)> {
		let count = rng.below(MAX_TAGS + 1);
		let (key_len, value_len) = if rng.below(4) == 0 { (MAX_TAG_KEY_BYTES, MAX_TAG_VALUE_BYTES) } else { (8, 12) };
		let mut keys = BTreeSet::new();
		while keys.len() < count {
			keys.insert(random_key(rng, key_len));
		}
		let mut pairs: Vec<(String, String)> = keys.into_iter().map(|key| (key, random_value(rng, value_len))).collect();
		rng.shuffle(&mut pairs);
		pairs
	}

	/// Pairs drawn from a tiny alphabet, so that distinct sets often share keys, values and
	/// prefixes: the cases a naive concatenation would confuse.
	fn small_alphabet_pairs(rng: &mut Rng) -> Vec<(String, String)> {
		const KEYS: &[&str] = &["a", "b", "ab", "a.b", "a-b", "_", "ba"];
		const VALUES: &[&str] = &["x", "y", "xy", "x=y", "a", "ab", "b", " "];
		let count = rng.below(5);
		let mut pairs = BTreeMap::new();
		while pairs.len() < count {
			pairs.insert((*rng.pick(KEYS)).to_owned(), (*rng.pick(VALUES)).to_owned());
		}
		pairs.into_iter().collect()
	}

	/// The canonical form computed independently of the implementation.
	fn reference_canonical(pairs: &[(String, String)]) -> Vec<u8> {
		let mut sorted: Vec<&(String, String)> = pairs.iter().collect();
		sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
		let mut out = Vec::new();
		for (i, (key, value)) in sorted.into_iter().enumerate() {
			if i > 0 {
				out.push(0x1E);
			}
			out.extend_from_slice(key.as_bytes());
			out.push(0x1F);
			out.extend_from_slice(value.as_bytes());
		}
		out
	}

	fn sorted_pairs(tags: &TagSet) -> Vec<(String, String)> {
		tags.iter().map(|(key, value)| (key.to_owned(), value.to_owned())).collect()
	}

	fn tags(pairs: &[(&str, &str)]) -> TagSet {
		TagSet::from_pairs(pairs.iter().copied()).expect("valid tags")
	}

	fn canonical_err(bytes: &[u8]) -> TagError {
		SeriesKey::from_canonical(bytes).expect_err("invalid canonical bytes")
	}

	fn pairs_err(pairs: &[(&str, &str)]) -> TagError {
		TagSet::from_pairs(pairs.iter().copied()).expect_err("invalid tags")
	}

	// ---- frozen constants and golden bytes ----

	#[test]
	fn frozen_constants_have_their_documented_values() {
		assert_eq!(MAX_TAG_KEY_BYTES, 128);
		assert_eq!(MAX_TAG_VALUE_BYTES, 256);
		assert_eq!(MAX_TAGS, 16);
		assert_eq!(MAX_SERIES_KEY_BYTES, 1024);
		assert_eq!(TAG_KEY_VALUE_SEPARATOR, 0x1F);
		assert_eq!(TAG_PAIR_SEPARATOR, 0x1E);
		assert_eq!(RESERVED_TAG_KEY_PREFIX, "__");
		assert_eq!(u32::from(KEY_VALUE_SEPARATOR_CHAR), u32::from(TAG_KEY_VALUE_SEPARATOR));
		assert_eq!(u32::from(PAIR_SEPARATOR_CHAR), u32::from(TAG_PAIR_SEPARATOR));
	}

	#[test]
	fn canonical_bytes_are_golden() {
		assert_eq!(tags(&[("region", "eu"), ("host", "a")]).series_key().as_bytes(), b"host\x1fa\x1eregion\x1feu");
		// Bytewise key order: uppercase before `_` before lowercase.
		assert_eq!(tags(&[("b", "1"), ("B", "2"), ("_", "3"), ("a", "4")]).series_key().as_bytes(), b"B\x1f2\x1e_\x1f3\x1ea\x1f4\x1eb\x1f1");
		// `-` < `.` < digits < uppercase < `_` < lowercase, and a prefix sorts first.
		let set = tags(&[("aa", "6"), ("a_", "5"), ("aA", "4"), ("a0", "3"), ("a.", "2"), ("a-", "1"), ("a", "0")]);
		assert_eq!(set.series_key().as_bytes(), b"a\x1f0\x1ea-\x1f1\x1ea.\x1f2\x1ea0\x1f3\x1eaA\x1f4\x1ea_\x1f5\x1eaa\x1f6");
		// A single tag has no pair separator.
		assert_eq!(tags(&[("k", "v")]).series_key().as_bytes(), b"k\x1fv");
		assert_eq!(tags(&[("k", "v")]).series_key().as_str(), "k\u{1f}v");
	}

	#[test]
	fn the_empty_set_is_series_zero() {
		let empty = TagSet::new();
		assert!(empty.is_empty());
		assert_eq!(empty.len(), 0);
		assert_eq!(empty.iter().next(), None);
		assert!(empty.series_key().is_empty());
		assert_eq!(empty.series_key().as_bytes(), b"");
		assert_eq!(TagSet::from_pairs(Vec::<(String, String)>::new()).expect("no pairs is the empty set"), empty);
		assert_eq!(SeriesKey::from_canonical(b"").expect("empty bytes"), SeriesKey::default());
		assert_eq!(TagSet::from(SeriesKey::default()), empty);
	}

	#[test]
	fn accessors_read_the_pairs() {
		let set = tags(&[("region", "eu"), ("host", "a"), ("dc", "x")]);
		assert_eq!(set.len(), 3);
		assert!(!set.is_empty());
		assert_eq!(set.get("host"), Some("a"));
		assert_eq!(set.get("dc"), Some("x"));
		assert_eq!(set.get("region"), Some("eu"));
		assert_eq!(set.get("rack"), None);
		assert_eq!(set.get("hos"), None);
		assert_eq!(set.get(""), None);
		assert_eq!(set.iter().collect::<Vec<_>>(), vec![("dc", "x"), ("host", "a"), ("region", "eu")]);
		assert_eq!((&set).into_iter().count(), 3);
		assert_eq!(format!("{set:?}"), r#"{"dc": "x", "host": "a", "region": "eu"}"#);
		assert_eq!(format!("{:?}", set.series_key()), r#"SeriesKey("dc\u{1f}x\u{1e}host\u{1f}a\u{1e}region\u{1f}eu")"#);
		assert_eq!(SeriesKey::from(set).len(), "dc\x1fx\x1ehost\x1fa\x1eregion\x1feu".len());
	}

	// ---- properties over generated tag sets ----

	#[test]
	fn canonical_bytes_do_not_depend_on_input_order() {
		let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
		let mut accepted = 0;
		for _ in 0..4_000 {
			let pairs = random_pairs(&mut rng);
			let expected = reference_canonical(&pairs);
			let result = TagSet::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
			if expected.len() > 1024 {
				assert_eq!(result, Err(TagError::SeriesKeyTooLong { len: expected.len() }));
				continue;
			}
			let set = result.expect("generated pairs are valid");
			assert_eq!(set.series_key().as_bytes(), expected.as_slice());
			let mut shuffled = pairs;
			rng.shuffle(&mut shuffled);
			let again = TagSet::from_pairs(shuffled).expect("same pairs, other order");
			assert_eq!(again, set);
			assert_eq!(again.series_key().as_bytes(), set.series_key().as_bytes());
			accepted += 1;
		}
		assert!(accepted > 2_000, "most generated sets fit the cap ({accepted})");
	}

	#[test]
	fn canonical_bytes_round_trip() {
		let mut rng = Rng(0xD1B5_4A32_D192_ED03);
		for _ in 0..4_000 {
			let pairs = random_pairs(&mut rng);
			let Ok(set) = TagSet::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))) else {
				continue;
			};
			let key = SeriesKey::from_canonical(set.series_key().as_bytes()).expect("canonical bytes read back");
			assert_eq!(&key, set.series_key());
			let back = TagSet::from(key);
			assert_eq!(back, set);
			let mut expected = pairs.clone();
			expected.sort();
			assert_eq!(sorted_pairs(&back), expected);
			assert_eq!(back.len(), pairs.len());
			for (key, value) in &pairs {
				assert_eq!(back.get(key), Some(value.as_str()));
			}
			// And through the owned conversions.
			assert_eq!(TagSet::from(SeriesKey::from(set.clone())), set);
		}
	}

	#[test]
	fn distinct_tag_sets_have_distinct_canonical_keys() {
		let mut rng = Rng(0x2545_F491_4F6C_DD1D);
		let mut seen: HashMap<Vec<u8>, Vec<(String, String)>> = HashMap::new();
		for i in 0..6_000 {
			let pairs = if i % 2 == 0 { small_alphabet_pairs(&mut rng) } else { random_pairs(&mut rng) };
			let Ok(set) = TagSet::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))) else {
				continue;
			};
			let mut sorted = pairs;
			sorted.sort();
			let bytes = set.series_key().as_bytes().to_vec();
			if let Some(previous) = seen.get(&bytes) {
				assert_eq!(previous, &sorted, "two different tag sets share canonical bytes {bytes:?}");
			} else {
				seen.insert(bytes, sorted);
			}
		}
		assert!(seen.len() > 3_000, "the generator produced enough distinct sets ({})", seen.len());
		// Shapes a separator-free concatenation would confuse stay apart.
		let confusable = [tags(&[("ab", "c")]), tags(&[("a", "bc")]), tags(&[("a", "b"), ("c", "d")]), tags(&[("a", "bc"), ("d", "e")]), tags(&[("a", "b")]), tags(&[("a", "b"), ("b", "a")])];
		let distinct: BTreeSet<&[u8]> = confusable.iter().map(|set| set.series_key().as_bytes()).collect();
		assert_eq!(distinct.len(), confusable.len());
	}

	#[test]
	fn ordering_is_canonical_byte_order_and_sorted_pair_order() {
		let mut rng = Rng(0xA076_1D64_78BD_642F);
		let sets: Vec<TagSet> = (0..600)
			.filter_map(|i| {
				let pairs = if i % 2 == 0 { small_alphabet_pairs(&mut rng) } else { random_pairs(&mut rng) };
				TagSet::from_pairs(pairs).ok()
			})
			.collect();
		for a in &sets {
			for b in &sets {
				let by_bytes = a.series_key().as_bytes().cmp(b.series_key().as_bytes());
				assert_eq!(a.cmp(b), by_bytes);
				assert_eq!(a.series_key().cmp(b.series_key()), by_bytes);
				assert_eq!(sorted_pairs(a).cmp(&sorted_pairs(b)), by_bytes, "{a:?} vs {b:?}");
				assert_eq!(a == b, by_bytes == Ordering::Equal);
			}
		}
	}

	// ---- boundaries: each uses the literal limit, so an off-by-one constant fails ----

	#[test]
	fn key_length_boundary_is_128_bytes() {
		let at_cap = format!("k{}", "x".repeat(127));
		assert_eq!(at_cap.len(), 128);
		let set = tags(&[(at_cap.as_str(), "v")]);
		assert_eq!(set.get(&at_cap), Some("v"));
		let mut canonical = at_cap.clone().into_bytes();
		canonical.extend_from_slice(b"\x1fv");
		assert_eq!(SeriesKey::from_canonical(&canonical).expect("128-byte key"), *set.series_key());

		let over = format!("k{}", "x".repeat(128));
		assert_eq!(over.len(), 129);
		assert_eq!(pairs_err(&[(over.as_str(), "v")]), TagError::KeyTooLong { key: at_cap.clone(), len: 129 });
		let mut canonical = over.clone().into_bytes();
		canonical.extend_from_slice(b"\x1fv");
		assert_eq!(canonical_err(&canonical), TagError::KeyTooLong { key: at_cap, len: 129 });
		assert_eq!(SeriesSelector::new([(over.as_str(), Some("v"))], false), Err(TagError::KeyTooLong { key: over[..128].to_owned(), len: 129 }));
	}

	#[test]
	fn value_length_boundary_is_256_bytes() {
		let at_cap = "v".repeat(256);
		let set = tags(&[("k", at_cap.as_str())]);
		assert_eq!(set.get("k"), Some(at_cap.as_str()));
		let canonical = format!("k\u{1f}{at_cap}");
		assert_eq!(SeriesKey::from_canonical(canonical.as_bytes()).expect("256-byte value"), *set.series_key());

		let over = "v".repeat(257);
		assert_eq!(pairs_err(&[("k", over.as_str())]), TagError::ValueTooLong { key: "k".to_owned(), len: 257 });
		assert_eq!(canonical_err(format!("k\u{1f}{over}").as_bytes()), TagError::ValueTooLong { key: "k".to_owned(), len: 257 });
		assert_eq!(SeriesSelector::new([("k", Some(over.as_str()))], false), Err(TagError::ValueTooLong { key: "k".to_owned(), len: 257 }));

		// The limit counts bytes, not characters: 128 two-byte characters fit, one more byte does not.
		let wide = "é".repeat(128);
		assert_eq!(wide.len(), 256);
		assert!(TagSet::from_pairs([("k", wide.as_str())]).is_ok());
		let wide_over = format!("{wide}a");
		assert_eq!(pairs_err(&[("k", wide_over.as_str())]), TagError::ValueTooLong { key: "k".to_owned(), len: 257 });
	}

	#[test]
	fn tag_count_boundary_is_16() {
		let names: Vec<String> = (0..17).map(|i| format!("k{i:02}")).collect();
		let sixteen: Vec<(&str, &str)> = names[..16].iter().map(|key| (key.as_str(), "v")).collect();
		let set = tags(&sixteen);
		assert_eq!(set.len(), 16);
		assert_eq!(SeriesKey::from_canonical(set.series_key().as_bytes()).expect("16 tags"), *set.series_key());

		let seventeen: Vec<(&str, &str)> = names.iter().map(|key| (key.as_str(), "v")).collect();
		assert_eq!(pairs_err(&seventeen), TagError::TooManyTags { count: 17 });
		let canonical = names.iter().map(|key| format!("{key}\u{1f}v")).collect::<Vec<_>>().join("\u{1e}");
		assert!(canonical.len() <= 1024, "only the count is over");
		assert_eq!(canonical_err(canonical.as_bytes()), TagError::TooManyTags { count: 17 });
		// The count is reported in full, not cut at the limit.
		let twenty: Vec<String> = (0..20).map(|i| format!("k{i:02}")).collect();
		assert_eq!(TagSet::from_pairs(twenty.iter().map(|key| (key.as_str(), "v"))), Err(TagError::TooManyTags { count: 20 }));
	}

	/// Four one-byte keys with values of 256, 256, 256 and `last` bytes: a canonical key of
	/// 4 * 2 + 3 + 768 + `last` = 779 + `last` bytes.
	fn four_wide_tags(last: usize) -> Vec<(String, String)> {
		let full = "v".repeat(256);
		vec![("a".to_owned(), full.clone()), ("b".to_owned(), full.clone()), ("c".to_owned(), full), ("d".to_owned(), "v".repeat(last))]
	}

	#[test]
	fn canonical_length_boundary_is_1024_bytes() {
		let at_cap = four_wide_tags(245);
		let set = TagSet::from_pairs(at_cap).expect("1024-byte canonical key");
		assert_eq!(set.series_key().len(), 1024);
		assert_eq!(SeriesKey::from_canonical(set.series_key().as_bytes()).expect("1024 bytes"), *set.series_key());

		let over = four_wide_tags(246);
		assert_eq!(TagSet::from_pairs(over.clone()), Err(TagError::SeriesKeyTooLong { len: 1025 }));
		let bytes = reference_canonical(&over);
		assert_eq!(bytes.len(), 1025);
		assert_eq!(canonical_err(&bytes), TagError::SeriesKeyTooLong { len: 1025 });
	}

	// ---- keys ----

	#[test]
	fn keys_follow_the_grammar() {
		for key in ["a", "Z", "_", "a0", "a_b", "a.b", "a-b", "A9_.-z", "_x", "_x__", "x__y"] {
			assert!(TagSet::from_pairs([(key, "v")]).is_ok(), "{key:?} is a valid key");
		}
	}

	#[test]
	fn an_invalid_first_byte_is_rejected() {
		for key in ["0a", "9", "-a", ".a", " a", "éa", "\u{1f}a", "=a", "/a"] {
			assert_eq!(pairs_err(&[(key, "v")]), TagError::InvalidKey { key: key.to_owned(), index: 0 }, "{key:?}");
			assert_eq!(SeriesSelector::new([(key, None::<&str>)], false), Err(TagError::InvalidKey { key: key.to_owned(), index: 0 }), "{key:?}");
		}
		// From canonical bytes, a byte that is not UTF-8 is quoted as U+FFFD.
		assert_eq!(canonical_err(b"\xff\x1fv"), TagError::InvalidKey { key: "\u{fffd}".to_owned(), index: 0 });
		assert_eq!(canonical_err(b"0\x1fv"), TagError::InvalidKey { key: "0".to_owned(), index: 0 });
	}

	#[test]
	fn an_invalid_later_byte_is_rejected() {
		for (key, index) in [("a b", 1), ("ab/c", 2), ("a:b", 1), ("a=b", 1), ("a,b", 1), ("aé", 1), ("abc\u{7f}", 3), ("a\u{1e}", 1), ("a+", 1), ("a*", 1)] {
			assert_eq!(pairs_err(&[(key, "v")]), TagError::InvalidKey { key: key.to_owned(), index }, "{key:?}");
		}
		assert_eq!(canonical_err(b"ab\xc3\xa9\x1fv"), TagError::InvalidKey { key: "abé".to_owned(), index: 2 });
		assert_eq!(canonical_err(b"a b\x1fv"), TagError::InvalidKey { key: "a b".to_owned(), index: 1 });
	}

	#[test]
	fn empty_keys_and_values_are_rejected() {
		assert_eq!(pairs_err(&[("", "v")]), TagError::EmptyKey);
		assert_eq!(pairs_err(&[("k", "")]), TagError::EmptyValue { key: "k".to_owned() });
		assert_eq!(canonical_err(b"\x1fv"), TagError::EmptyKey);
		assert_eq!(canonical_err(b"k\x1f"), TagError::EmptyValue { key: "k".to_owned() });
		assert_eq!(SeriesSelector::new([("", Some("v"))], false), Err(TagError::EmptyKey));
		assert_eq!(SeriesSelector::new([("k", Some(""))], false), Err(TagError::EmptyValue { key: "k".to_owned() }));
	}

	#[test]
	fn the_reserved_prefix_is_input_only() {
		for key in ["__", "__x", "__x_y", "___"] {
			assert_eq!(pairs_err(&[(key, "v")]), TagError::ReservedKey { key: key.to_owned() });
			assert_eq!(SeriesSelector::new([(key, Some("v"))], false), Err(TagError::ReservedKey { key: key.to_owned() }));
			assert_eq!(SeriesSelector::new([(key, None::<&str>)], true), Err(TagError::ReservedKey { key: key.to_owned() }));
		}
		// The storage path accepts a reserved key, still checking everything else.
		let stored = SeriesKey::from_canonical(b"__dim\x1fs\x1ehost\x1fa").expect("a later version's system dimension");
		let set = TagSet::from(stored.clone());
		assert_eq!(set.get("__dim"), Some("s"));
		assert_eq!(set.len(), 2);
		assert_eq!(SeriesKey::from_canonical(stored.as_bytes()).expect("round trip"), stored);
		assert_eq!(canonical_err(b"__\x1f"), TagError::EmptyValue { key: "__".to_owned() });
		assert_eq!(canonical_err(b"__d m\x1fs"), TagError::InvalidKey { key: "__d m".to_owned(), index: 3 });
		// Selectors cannot name it: non-exact selectors see past it, exact ones never match it.
		assert!(SeriesSelector::all().matches(&set));
		assert!(SeriesSelector::new([("host", Some("a"))], false).expect("valid").matches(&set));
		assert!(!SeriesSelector::new([("host", Some("a"))], true).expect("valid").matches(&set));
		assert!(!SeriesSelector::untagged().matches(&set));
	}

	// ---- values ----

	#[test]
	fn control_characters_in_values_are_rejected() {
		for byte in (0x00_u8..=0x1F).chain([0x7F]) {
			let c = char::from(byte);
			for (value, index) in [(format!("{c}"), 0), (format!("a{c}b"), 1), (format!("ab{c}"), 2)] {
				let expected = TagError::ControlCharacter { key: "k".to_owned(), index, character: c };
				assert_eq!(TagSet::from_pairs([("k", value.as_str())]), Err(expected.clone()), "U+{:04X}", u32::from(byte));
				assert_eq!(SeriesSelector::new([("k", Some(value.as_str()))], false), Err(expected.clone()));
				// 0x1E splits pairs in canonical bytes, so it is a different error there.
				if byte != 0x1E {
					assert_eq!(canonical_err(format!("k\u{1f}{value}").as_bytes()), expected, "U+{:04X}", u32::from(byte));
				}
			}
		}
	}

	#[test]
	fn other_characters_are_allowed_without_normalization() {
		for value in [" ", "a b", "=", ",", "\"", "\\", "{}", "~", "é", "中文", "😀", "\u{80}", "\u{85}", "\u{9f}", "\u{a0}", "\u{feff}", "\u{2028}"] {
			let set = TagSet::from_pairs([("k", value)]).unwrap_or_else(|e| panic!("{value:?} is a valid value: {e}"));
			assert_eq!(set.get("k"), Some(value));
			assert_eq!(TagSet::from(SeriesKey::from_canonical(set.series_key().as_bytes()).expect("round trip")), set);
		}
		// NFC and NFD spellings of `é` are distinct values.
		let nfc = tags(&[("k", "\u{e9}")]);
		let nfd = tags(&[("k", "e\u{301}")]);
		assert_ne!(nfc, nfd);
		assert_ne!(nfc.series_key().as_bytes(), nfd.series_key().as_bytes());
	}

	#[test]
	fn values_that_are_not_utf8_are_rejected() {
		assert_eq!(canonical_err(b"k\x1f\xff"), TagError::ValueNotUtf8 { key: "k".to_owned(), index: 0 });
		assert_eq!(canonical_err(b"k\x1fx\xc3"), TagError::ValueNotUtf8 { key: "k".to_owned(), index: 1 });
		// An overlong encoding and a surrogate.
		assert_eq!(canonical_err(b"k\x1fab\xc0\xaf"), TagError::ValueNotUtf8 { key: "k".to_owned(), index: 2 });
		assert_eq!(canonical_err(b"k\x1f\xed\xa0\x80"), TagError::ValueNotUtf8 { key: "k".to_owned(), index: 0 });
		// In a later pair, after valid ones.
		assert_eq!(canonical_err(b"a\x1f1\x1eb\x1f\x80"), TagError::ValueNotUtf8 { key: "b".to_owned(), index: 0 });
	}

	// ---- sets and canonical structure ----

	#[test]
	fn duplicate_keys_are_rejected_from_pairs() {
		assert_eq!(pairs_err(&[("a", "1"), ("b", "2"), ("a", "3")]), TagError::DuplicateKey { key: "a".to_owned() });
		assert_eq!(pairs_err(&[("a", "1"), ("a", "1")]), TagError::DuplicateKey { key: "a".to_owned() });
		// A per-pair error comes before the duplicate.
		assert_eq!(pairs_err(&[("a", "1"), ("a", "1"), ("b", "")]), TagError::EmptyValue { key: "b".to_owned() });
	}

	#[test]
	fn duplicate_keys_are_rejected_from_canonical() {
		assert_eq!(canonical_err(b"a\x1f1\x1ea\x1f2"), TagError::DuplicateKey { key: "a".to_owned() });
		assert_eq!(canonical_err(b"a\x1f1\x1ea\x1f1"), TagError::DuplicateKey { key: "a".to_owned() });
		assert_eq!(canonical_err(b"a\x1f1\x1eb\x1f2\x1eb\x1f3"), TagError::DuplicateKey { key: "b".to_owned() });
	}

	#[test]
	fn unsorted_canonical_bytes_are_rejected() {
		assert_eq!(canonical_err(b"b\x1f1\x1ea\x1f2"), TagError::UnsortedKeys { key: "a".to_owned(), previous: "b".to_owned() });
		// A key sorts after its own prefix.
		assert_eq!(canonical_err(b"ab\x1f1\x1ea\x1f2"), TagError::UnsortedKeys { key: "a".to_owned(), previous: "ab".to_owned() });
		// Bytewise, not case-insensitive: `a` (0x61) sorts after `B` (0x42).
		assert_eq!(canonical_err(b"a\x1f1\x1eB\x1f2"), TagError::UnsortedKeys { key: "B".to_owned(), previous: "a".to_owned() });
		assert!(SeriesKey::from_canonical(b"B\x1f2\x1ea\x1f1").is_ok());
	}

	#[test]
	fn malformed_canonical_bytes_are_rejected() {
		assert_eq!(canonical_err(b"a"), TagError::MissingSeparator { pair: 0 });
		assert_eq!(canonical_err(b"\x1e"), TagError::MissingSeparator { pair: 0 });
		assert_eq!(canonical_err(b"\x1ea\x1f1"), TagError::MissingSeparator { pair: 0 });
		assert_eq!(canonical_err(b"a\x1f1\x1e"), TagError::MissingSeparator { pair: 1 });
		assert_eq!(canonical_err(b"a\x1f1\x1e\x1eb\x1f2"), TagError::MissingSeparator { pair: 1 });
		assert_eq!(canonical_err(b"a\x1f1\x1eb"), TagError::MissingSeparator { pair: 1 });
		// A second 0x1F belongs to the value, where it is a control character.
		assert_eq!(canonical_err(b"a\x1f1\x1f2"), TagError::ControlCharacter { key: "a".to_owned(), index: 1, character: '\u{1f}' });
		// The length cap is checked before anything is parsed.
		assert_eq!(canonical_err(&[0xFF; 1025]), TagError::SeriesKeyTooLong { len: 1025 });
	}

	// ---- selectors ----

	#[test]
	fn selector_truth_table() {
		let sets = [tags(&[]), tags(&[("host", "a")]), tags(&[("host", "a"), ("dc", "x")]), tags(&[("host", "b")]), tags(&[("dc", "x")])];
		let selector = |matchers: &[(&str, Option<&str>)], exact: bool| SeriesSelector::new(matchers.iter().copied(), exact).expect("valid selector");
		// Columns: {}, {host=a}, {host=a, dc=x}, {host=b}, {dc=x}.
		let table: [(SeriesSelector, [bool; 5]); 16] = [(SeriesSelector::all(), [true, true, true, true, true]), (SeriesSelector::default(), [true, true, true, true, true]), (SeriesSelector::untagged(), [true, false, false, false, false]), (selector(&[], true), [true, false, false, false, false]), (selector(&[("host", Some("a"))], false), [false, true, true, false, false]), (selector(&[("host", Some("a"))], true), [false, true, false, false, false]), (selector(&[("host", Some("a")), ("dc", None)], false), [false, true, false, false, false]), (selector(&[("host", Some("a")), ("dc", None)], true), [false, true, false, false, false]), (selector(&[("dc", None)], false), [true, true, false, true, false]), (selector(&[("dc", None)], true), [true, false, false, false, false]), (selector(&[("host", Some("a")), ("dc", Some("x"))], false), [false, false, true, false, false]), (selector(&[("dc", Some("x")), ("host", Some("a"))], true), [false, false, true, false, false]), (selector(&[("host", Some("a")), ("dc", Some("y"))], false), [false, false, false, false, false]), (selector(&[("host", None), ("dc", None)], false), [true, false, false, false, false]), (selector(&[("dc", Some("x"))], false), [false, false, true, false, true]), (selector(&[("rack", Some("r"))], false), [false, false, false, false, false])];
		for (selector, expected) in &table {
			let got: Vec<bool> = sets.iter().map(|set| selector.matches(set)).collect();
			assert_eq!(got, expected.to_vec(), "{selector:?}");
		}
	}

	#[test]
	fn selector_matches_agrees_with_its_definition() {
		const KEYS: &[&str] = &["a", "b", "ab", "c"];
		const VALUES: &[&str] = &["x", "y"];
		let mut rng = Rng(0x0BAD_5EED_1234_5678);
		for _ in 0..3_000 {
			let set = TagSet::from_pairs(small_alphabet_pairs(&mut rng)).expect("valid");
			let mut matchers = BTreeMap::new();
			for _ in 0..rng.below(4) {
				let value = if rng.below(3) == 0 { None } else { Some(*rng.pick(VALUES)) };
				matchers.insert(*rng.pick(KEYS), value);
			}
			let exact = rng.below(2) == 0;
			let selector = SeriesSelector::new(matchers.clone(), exact).expect("valid selector");
			let model: BTreeMap<&str, &str> = set.iter().collect();
			let every_matcher_holds = matchers.iter().all(|(key, value)| model.get(key).copied() == *value);
			let valued: BTreeMap<&str, &str> = matchers.iter().filter_map(|(key, value)| value.map(|value| (*key, value))).collect();
			let expected = every_matcher_holds && (!exact || model == valued);
			assert_eq!(selector.matches(&set), expected, "{selector:?} on {set:?}");
		}
	}

	#[test]
	fn selectors_are_validated_and_order_independent() {
		let one = SeriesSelector::new([("host", Some("a")), ("dc", None)], true).expect("valid");
		let two = SeriesSelector::new([("dc", None), ("host", Some("a"))], true).expect("valid");
		assert_eq!(one, two);
		assert!(one.is_exact());
		assert!(!SeriesSelector::all().is_exact());
		assert!(SeriesSelector::untagged().is_exact());
		assert_eq!(one.matchers().collect::<Vec<_>>(), vec![("dc", None), ("host", Some("a"))]);
		assert_eq!(one.matchers().len(), 2);
		assert_eq!(SeriesSelector::all().matchers().len(), 0);
		assert_ne!(one, SeriesSelector::new([("host", Some("a")), ("dc", None)], false).expect("valid"));

		assert_eq!(SeriesSelector::new([("host", Some("a")), ("host", Some("b"))], false), Err(TagError::DuplicateKey { key: "host".to_owned() }));
		assert_eq!(SeriesSelector::new([("host", Some("a")), ("host", None)], false), Err(TagError::DuplicateKey { key: "host".to_owned() }));
		assert_eq!(SeriesSelector::new([("host", None::<&str>), ("host", None)], false), Err(TagError::DuplicateKey { key: "host".to_owned() }));
		assert_eq!(SeriesSelector::new([("1host", None::<&str>)], false), Err(TagError::InvalidKey { key: "1host".to_owned(), index: 0 }));
		assert_eq!(SeriesSelector::new([("k", Some("a\nb"))], false), Err(TagError::ControlCharacter { key: "k".to_owned(), index: 1, character: '\n' }));
		// No cap on matchers beyond one per key: absent-tag matchers may name many keys.
		let many: Vec<String> = (0..40).map(|i| format!("k{i}")).collect();
		let absent = SeriesSelector::new(many.iter().map(|key| (key.as_str(), None::<&str>)), false).expect("valid");
		assert!(absent.matches(&tags(&[("host", "a")])));
	}

	// ---- errors ----

	#[test]
	fn errors_name_the_key_and_the_limit() {
		let long_key = "k".repeat(300);
		let cases: [(TagError, &[&str]); 14] = [(pairs_err(&[("", "v")]), &["empty"]), (pairs_err(&[(long_key.as_str(), "v")]), &["300 bytes", "128"]), (pairs_err(&[("1a", "v")]), &["\"1a\"", "start"]), (pairs_err(&[("a b", "v")]), &["\"a b\"", "byte 1"]), (pairs_err(&[("__a", "v")]), &["\"__a\"", "reserved"]), (pairs_err(&[("k", "")]), &["\"k\"", "empty"]), (pairs_err(&[("k", &"v".repeat(300))]), &["\"k\"", "300 bytes", "256"]), (pairs_err(&[("k", "a\tb")]), &["\"k\"", "U+0009", "byte 1"]), (canonical_err(b"k\x1f\xff"), &["\"k\"", "UTF-8"]), (pairs_err(&[("k", "1"), ("k", "2")]), &["\"k\"", "more than once"]), (TagSet::from_pairs((0..17).map(|i| (format!("k{i}"), "v"))).expect_err("17 tags"), &["17 tags", "16"]), (TagSet::from_pairs(four_wide_tags(246)).expect_err("1025 bytes"), &["1025 bytes", "1024"]), (canonical_err(b"b\x1f1\x1ea\x1f2"), &["\"a\"", "\"b\""]), (canonical_err(b"a\x1f1\x1eb"), &["pair 1", "0x1F"])];
		for (error, needles) in cases {
			let message = error.to_string();
			for needle in needles {
				assert!(message.contains(needle), "{message:?} should contain {needle:?}");
			}
		}
		// A long key is quoted cut to 128 bytes, and a control character in a key is escaped.
		let message = pairs_err(&[(long_key.as_str(), "v")]).to_string();
		assert!(!message.contains(&"k".repeat(129)), "{message}");
		assert!(pairs_err(&[("a\u{1b}[2J", "v")]).to_string().contains("\\u{1b}"));
	}

	#[test]
	fn quoted_keys_are_cut_at_a_character_boundary() {
		// 127 ASCII bytes and a two-byte character straddle the 128-byte cut.
		let key = format!("{}é", "k".repeat(127));
		assert_eq!(key_for_error(key.as_bytes()), "k".repeat(127));
		assert_eq!(key_for_error(b"ab"), "ab");
		assert_eq!(key_for_error(b"a\xffb"), "a\u{fffd}b");
	}

	#[test]
	fn tag_error_is_a_std_error() {
		fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}
		assert_error::<TagError>();
	}
}
