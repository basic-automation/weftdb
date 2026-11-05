//! An aspect's optional **upsert-key declaration** (roadmap Phase 4.6, correctness
//! semantics), in the honest narrowed form the tags design allows.
//!
//! Dedup identity in WeftDB is `(timestamp, series)`: two rows of one series at one
//! timestamp resolve newer-wins, at read and at reconcile. A series is a full tag set, and
//! the store keeps no per-row column besides the one value, so the only upsert key an
//! aspect can honestly offer is `timestamp` plus a set `K` of tag keys **that every row's
//! tags fall inside**. Declaring `K` therefore narrows what an ingest may carry: a
//! [`TagSet`] naming a key outside `K` is refused
//! ([`StoreError::NotAnUpsertKey`](crate::StoreError::NotAnUpsertKey)), because a tag
//! outside the key would have nowhere to live once two rows that differ only in it were
//! folded into one. Within `K` a row's tags restricted to `K` are its full tag set, so the
//! identity is `(timestamp, tags ∩ K)` exactly. With no declaration the identity is
//! `(timestamp, full tag set)`, the behaviour of every aspect before this type.
//!
//! The declaration is immutable once made, like the schema beside it: re-declaring with
//! different keys is a conflict.

use std::collections::BTreeSet;

use crate::{TagError, TagSet};

/// The token a wire form of [`UpsertKeys`] names the timestamp with. The timestamp is
/// always part of the key; it is spelled out so that the declaration reads as the
/// identity it is.
pub const TIMESTAMP_UPSERT_KEY: &str = "timestamp";

/// An aspect's declared upsert key: the timestamp and a set of tag keys (see the
/// [module documentation](self)).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UpsertKeys {
	tags: BTreeSet<String>,
}

impl UpsertKeys {
	/// The key `timestamp` plus the tag keys `tags`, in any order. No tag keys is a valid
	/// declaration: only untagged rows (series 0) may then be ingested.
	///
	/// # Errors
	///
	/// A [`TagError`] when a key breaks the tag-key grammar (empty, too long, an invalid
	/// byte, a reserved `__` prefix), is repeated, or there are more than
	/// [`MAX_TAGS`](crate::tags::MAX_TAGS) of them.
	pub fn new<I, K>(tags: I) -> Result<Self, TagError>
	where
		I: IntoIterator<Item = K>,
		K: AsRef<str>,
	{
		let keys: Vec<String> = tags.into_iter().map(|key| key.as_ref().to_string()).collect();
		// The tag set's own checks are the key grammar: a placeholder value per key.
		TagSet::from_pairs(keys.iter().map(|key| (key.as_str(), "k")))?;
		Ok(Self { tags: keys.into_iter().collect() })
	}

	/// The tag keys of the declaration, in ascending order (the timestamp is implied).
	pub fn tag_keys(&self) -> impl Iterator<Item = &str> {
		self.tags.iter().map(String::as_str)
	}

	/// The first key of `tags` (in key order) that the declaration does not name, or
	/// [`None`] when every one is an upsert key.
	#[must_use]
	pub fn first_outside<'a>(&self, tags: &'a TagSet) -> Option<&'a str> {
		tags.iter().map(|(key, _)| key).find(|key| !self.tags.contains(*key))
	}

	/// The declaration's wire form: [`TIMESTAMP_UPSERT_KEY`] first, then the tag keys in
	/// ascending order. What the catalog stores (as a JSON array) and the server echoes.
	#[must_use]
	pub fn to_wire(&self) -> Vec<String> {
		std::iter::once(TIMESTAMP_UPSERT_KEY.to_string()).chain(self.tags.iter().cloned()).collect()
	}

	/// Parse a wire form: [`TIMESTAMP_UPSERT_KEY`] exactly once, anywhere, and tag keys.
	///
	/// # Errors
	///
	/// A message naming the problem: no `timestamp`, `timestamp` twice, or a tag key the
	/// [`new`](Self::new) checks refuse.
	pub fn from_wire<I, K>(keys: I) -> Result<Self, String>
	where
		I: IntoIterator<Item = K>,
		K: AsRef<str>,
	{
		let mut timestamp = 0_usize;
		let mut tags = Vec::new();
		for key in keys {
			let key = key.as_ref();
			if key == TIMESTAMP_UPSERT_KEY {
				timestamp += 1;
			} else {
				tags.push(key.to_string());
			}
		}
		match timestamp {
			0 => return Err(format!("upsert keys must include {TIMESTAMP_UPSERT_KEY:?}, which every upsert key starts from")),
			1 => {}
			_ => return Err(format!("upsert keys name {TIMESTAMP_UPSERT_KEY:?} more than once")),
		}
		Self::new(tags).map_err(|e| format!("upsert key: {e}"))
	}

	/// The catalog's text for the declaration: the [wire form](Self::to_wire) as JSON.
	pub(crate) fn to_catalog(&self) -> String {
		serde_json::to_string(&self.to_wire()).unwrap_or_default()
	}

	/// Read the catalog's text back.
	pub(crate) fn from_catalog(text: &str) -> anyhow::Result<Self> {
		let keys: Vec<String> = serde_json::from_str(text)?;
		Self::from_wire(keys).map_err(anyhow::Error::msg)
	}
}

impl std::fmt::Display for UpsertKeys {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "[{}]", self.to_wire().join(", "))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_wire_form_round_trips_and_needs_one_timestamp() {
		let keys = UpsertKeys::from_wire(["host", "timestamp", "dc"]).expect("parses");
		assert_eq!(keys.to_wire(), vec!["timestamp", "dc", "host"]);
		assert_eq!(UpsertKeys::from_catalog(&keys.to_catalog()).expect("reads back"), keys);
		assert!(UpsertKeys::from_wire(["host"]).is_err());
		assert!(UpsertKeys::from_wire(["timestamp", "timestamp"]).is_err());
		assert!(UpsertKeys::from_wire(["timestamp", "__dim"]).is_err());
		assert!(UpsertKeys::from_wire(["timestamp", "host", "host"]).is_err());
		assert_eq!(UpsertKeys::from_wire(["timestamp"]).expect("parses").tag_keys().count(), 0);
	}

	#[test]
	fn first_outside_names_a_tag_the_declaration_lacks() {
		let keys = UpsertKeys::new(["host"]).expect("valid");
		assert_eq!(keys.first_outside(&TagSet::from_pairs([("host", "a")]).expect("valid")), None);
		assert_eq!(keys.first_outside(&TagSet::new()), None);
		assert_eq!(keys.first_outside(&TagSet::from_pairs([("host", "a"), ("region", "eu")]).expect("valid")), Some("region"));
	}
}
