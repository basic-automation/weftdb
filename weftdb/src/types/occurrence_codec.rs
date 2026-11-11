//! Lossless compact form of a pattern's occurrence list, in memory and on disk.
//!
//! A dictionary pattern can collect hundreds of thousands of occurrences, and each
//! [`Occurrence`] carries its aspect, resolution, size and a full [`DatabaseInfo`] (the
//! whole subject map). Those fields are nearly always the same for every occurrence of a
//! pattern, and the timestamps are nearly always evenly spaced, so [`OccurrenceLog`]
//! factors the constant fields into a small table of headers and keeps the timestamps as
//! varints, about three bytes per occurrence. The stored payload is the same layout:
//!
//! ```text
//! u8   version (2)
//! var  header count H
//! H × { var len, len bytes of JSON [aspect, resolution, size, database_info, pattern_id] }
//! var  occurrence count N
//! N × { var header index, zigzag var Δ²(beginning ns), zigzag var Δ(duration ns) }
//! ```
//!
//! Times are kept as `i128` nanoseconds, so every `DateTime<Utc>` round-trips exactly.
//! Order, duplicates and every field round-trip: `decode(&encode(x)) == x`.

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use splimes::Resolution;

use crate::{AspectId, DatabaseInfo, Occurrence, PatternID};

const VERSION: u8 = 2;

/// The codec id stored beside each payload, so a later layout can coexist with this one.
pub const CODEC_V1: i64 = 1;

type Header = (AspectId, Resolution, usize, DatabaseInfo, PatternID);

fn put_var(out: &mut Vec<u8>, mut v: u128) {
	while v >= 0x80 {
		out.push(low_byte(v) | 0x80);
		v >>= 7;
	}
	out.push(low_byte(v));
}

const fn low_byte(v: u128) -> u8 {
	v.to_le_bytes()[0] & 0x7f
}

// Zigzag reinterprets the bits; no value is lost in either direction.
#[allow(clippy::cast_sign_loss)]
const fn zigzag(v: i128) -> u128 {
	((v << 1) ^ (v >> 127)) as u128
}

#[allow(clippy::cast_possible_wrap)]
const fn unzigzag(v: u128) -> i128 {
	((v >> 1) as i128) ^ -((v & 1) as i128)
}

fn nanos(t: &DateTime<Utc>) -> i128 {
	i128::from(t.timestamp()) * 1_000_000_000 + i128::from(t.timestamp_subsec_nanos())
}

fn from_nanos(n: i128) -> Result<DateTime<Utc>> {
	let secs = i64::try_from(n.div_euclid(1_000_000_000)).context("occurrence time out of range")?;
	let sub = u32::try_from(n.rem_euclid(1_000_000_000)).context("occurrence time out of range")?;
	DateTime::from_timestamp(secs, sub).context("occurrence time out of range")
}

struct Reader<'a> {
	bytes: &'a [u8],
	at: usize,
}

impl Reader<'_> {
	fn var(&mut self) -> Result<u128> {
		let mut v = 0_u128;
		for shift in (0..128).step_by(7) {
			let byte = *self.bytes.get(self.at).context("occurrence payload ends inside a varint")?;
			self.at += 1;
			v |= u128::from(byte & 0x7f) << shift;
			if byte & 0x80 == 0 {
				return Ok(v);
			}
		}
		bail!("occurrence payload has an over-long varint")
	}

	fn usize(&mut self) -> Result<usize> {
		Ok(usize::try_from(self.var()?)?)
	}

	fn take(&mut self, len: usize) -> Result<&[u8]> {
		let end = self.at.checked_add(len).filter(|end| *end <= self.bytes.len()).context("occurrence payload ends inside a header")?;
		let slice = &self.bytes[self.at..end];
		self.at = end;
		Ok(slice)
	}
}

/// Delta state carried from one occurrence to the next.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Cursor {
	begin: i128,
	delta: i128,
	duration: i128,
}

/// An append-only, compact, lossless list of occurrences.
///
/// [`OccurrenceLog::as_vec`] materializes the full list on first use and keeps it until
/// the next append; code that only counts, appends, persists or iterates should use
/// [`len`](Self::len), [`push`](Self::push), [`encode`](Self::encode) and
/// [`iter`](Self::iter), which never materialize it.
#[derive(Default)]
pub struct OccurrenceLog {
	headers: Vec<Header>,
	body: Vec<u8>,
	len: usize,
	tail: Cursor,
	cache: OnceLock<Vec<Occurrence>>,
}

impl OccurrenceLog {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	#[must_use]
	pub const fn len(&self) -> usize {
		self.len
	}

	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.len == 0
	}

	/// Bytes held for the list, excluding any materialized copy.
	#[must_use]
	pub const fn compact_bytes(&self) -> usize {
		self.body.capacity() + self.headers.capacity() * std::mem::size_of::<Header>()
	}

	pub fn push(&mut self, occurrence: &Occurrence) {
		let header: Header = (*occurrence.aspect(), *occurrence.resolution(), occurrence.size(), occurrence.database_info().clone(), *occurrence.pattern_id());
		let index = self.headers.iter().rposition(|h| *h == header).unwrap_or_else(|| {
			self.headers.push(header);
			self.headers.len() - 1
		});
		let begin = nanos(occurrence.beginning());
		let duration = nanos(occurrence.end()) - begin;
		let delta = begin - self.tail.begin;
		put_var(&mut self.body, index as u128);
		put_var(&mut self.body, zigzag(delta - self.tail.delta));
		put_var(&mut self.body, zigzag(duration - self.tail.duration));
		self.tail = Cursor { begin, delta, duration };
		self.len += 1;
		self.cache = OnceLock::new();
	}

	/// Decode the occurrences one at a time, in order.
	///
	/// # Panics
	///
	/// Never in practice: the body is only ever written by [`push`](Self::push) or checked
	/// in full by [`decode`](Self::decode).
	pub fn iter(&self) -> impl Iterator<Item = Occurrence> + '_ {
		let mut r = Reader { bytes: &self.body, at: 0 };
		let mut cur = Cursor::default();
		(0..self.len).map(move |_| {
			// The body is only ever written by `push`, so it always decodes.
			let index = r.usize().expect("occurrence log body is well-formed");
			cur.delta += unzigzag(r.var().expect("occurrence log body is well-formed"));
			cur.begin += cur.delta;
			cur.duration += unzigzag(r.var().expect("occurrence log body is well-formed"));
			self.build(index, cur).expect("occurrence log body is well-formed")
		})
	}

	fn build(&self, index: usize, cur: Cursor) -> Result<Occurrence> {
		let (aspect, resolution, size, database_info, pattern_id) = self.headers.get(index).with_context(|| format!("occurrence names header {index} of {}", self.headers.len()))?.clone();
		Ok(Occurrence::new(aspect, resolution, size, database_info, pattern_id, from_nanos(cur.begin)?, from_nanos(cur.begin + cur.duration)?))
	}

	#[must_use]
	pub fn first(&self) -> Option<Occurrence> {
		self.iter().next()
	}

	/// The full list, materialized on first call and cached until the next append.
	pub fn as_vec(&self) -> &Vec<Occurrence> {
		self.cache.get_or_init(|| self.iter().collect())
	}

	/// The stored payload.
	///
	/// # Errors
	///
	/// When a header cannot be serialized.
	pub fn encode(&self) -> Result<Vec<u8>> {
		let mut out = vec![VERSION];
		put_var(&mut out, self.headers.len() as u128);
		for header in &self.headers {
			let json = serde_json::to_vec(header)?;
			put_var(&mut out, json.len() as u128);
			out.extend_from_slice(&json);
		}
		put_var(&mut out, self.len as u128);
		out.extend_from_slice(&self.body);
		Ok(out)
	}

	/// Parse a payload written by [`encode`](Self::encode), checking every entry.
	///
	/// # Errors
	///
	/// When the payload is truncated, from an unknown version, or names a missing header.
	pub fn decode(bytes: &[u8]) -> Result<Self> {
		let Some((&version, rest)) = bytes.split_first() else { bail!("empty occurrence payload") };
		if version != VERSION {
			bail!("unknown occurrence payload version {version}");
		}
		let mut r = Reader { bytes: rest, at: 0 };
		let header_count = r.usize()?;
		let mut headers: Vec<Header> = Vec::with_capacity(header_count.min(1024));
		for _ in 0..header_count {
			let len = r.usize()?;
			headers.push(serde_json::from_slice(r.take(len)?).context("occurrence header is not valid JSON")?);
		}
		let len = r.usize()?;
		let body_start = r.at;
		let mut log = Self { headers, ..Self::default() };
		let mut cur = Cursor::default();
		for _ in 0..len {
			let index = r.usize()?;
			cur.delta = cur.delta.checked_add(unzigzag(r.var()?)).context("occurrence time out of range")?;
			cur.begin = cur.begin.checked_add(cur.delta).context("occurrence time out of range")?;
			cur.duration = cur.duration.checked_add(unzigzag(r.var()?)).context("occurrence time out of range")?;
			log.build(index, cur)?;
		}
		if r.at != rest.len() {
			bail!("occurrence payload has {} trailing bytes", rest.len() - r.at);
		}
		log.body = rest[body_start..].to_vec();
		log.len = len;
		log.tail = cur;
		Ok(log)
	}

	/// Append every occurrence of `other`, in order.
	pub fn extend_from(&mut self, other: &Self) {
		for occurrence in other.iter() {
			self.push(&occurrence);
		}
	}
}

impl FromIterator<Occurrence> for OccurrenceLog {
	fn from_iter<I: IntoIterator<Item = Occurrence>>(iter: I) -> Self {
		let mut log = Self::new();
		for occurrence in iter {
			log.push(&occurrence);
		}
		log
	}
}

impl From<Vec<Occurrence>> for OccurrenceLog {
	fn from(list: Vec<Occurrence>) -> Self {
		list.into_iter().collect()
	}
}

impl Clone for OccurrenceLog {
	fn clone(&self) -> Self {
		Self { headers: self.headers.clone(), body: self.body.clone(), len: self.len, tail: self.tail, cache: OnceLock::new() }
	}
}

// Headers are numbered by first appearance, so equal sequences have equal encodings.
impl PartialEq for OccurrenceLog {
	fn eq(&self, other: &Self) -> bool {
		self.len == other.len && self.body == other.body && self.headers == other.headers
	}
}

impl Eq for OccurrenceLog {}

impl std::fmt::Debug for OccurrenceLog {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("OccurrenceLog").field("len", &self.len).field("headers", &self.headers.len()).field("bytes", &self.body.len()).finish_non_exhaustive()
	}
}

// Same wire shape as `Vec<Occurrence>`, so serialized patterns are unchanged.
impl Serialize for OccurrenceLog {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.collect_seq(self.iter())
	}
}

impl<'de> Deserialize<'de> for OccurrenceLog {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		Ok(Vec::<Occurrence>::deserialize(deserializer)?.into())
	}
}

/// Encode `occurrences` losslessly, preserving order.
///
/// # Errors
///
/// When a header cannot be serialized.
pub fn encode(occurrences: &[Occurrence]) -> Result<Vec<u8>> {
	occurrences.iter().cloned().collect::<OccurrenceLog>().encode()
}

/// Decode a payload written by [`encode`].
///
/// # Errors
///
/// When the payload is truncated, from an unknown version, or names a missing header.
pub fn decode(bytes: &[u8]) -> Result<Vec<Occurrence>> {
	Ok(OccurrenceLog::decode(bytes)?.iter().collect())
}

#[cfg(test)]
#[allow(clippy::significant_drop_tightening)]
mod tests {
	use chrono::TimeZone;

	use super::*;

	fn info(name: &str) -> DatabaseInfo {
		DatabaseInfo::new(name.to_string(), format!("/data/{name}"))
	}

	// One aspect for the whole test run: `AspectId::default()` is a fresh random id.
	fn aspect() -> AspectId {
		static ASPECT: OnceLock<AspectId> = OnceLock::new();
		*ASPECT.get_or_init(AspectId::default)
	}

	fn occ(info: &DatabaseInfo, pattern: PatternID, begin_ms: i64, len_ms: i64, size: usize) -> Occurrence {
		let b = Utc.timestamp_millis_opt(begin_ms).unwrap();
		Occurrence::new(aspect(), Resolution::Minutes, size, info.clone(), pattern, b, b + chrono::Duration::milliseconds(len_ms))
	}

	#[test]
	fn round_trips_order_duplicates_and_mixed_headers() -> Result<()> {
		let (a, b) = (info("a"), info("b"));
		let p = PatternID::new();
		let base = Utc.with_ymd_and_hms(2026, 11, 6, 0, 0, 0).unwrap().timestamp_millis();
		let mut list: Vec<Occurrence> = (0..500).map(|i| occ(&a, p, base + i * 60_000, 600_000, 10)).collect();
		list.push(occ(&b, p, base - 5, 1, 3)); // out of order, other header
		list.push(list[7].clone()); // duplicate
		list.push(occ(&a, p, i64::from(i32::MIN), -7, 10)); // negative, far jump
		let s = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::nanoseconds(123_456_789);
		list.push(Occurrence::new(aspect(), Resolution::Minutes, 1, a.clone(), p, s, s + chrono::Duration::nanoseconds(1))); // sub-millisecond
		list.push(Occurrence::new(aspect(), Resolution::Minutes, 1, a, p, DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC)); // extremes
		assert_eq!(decode(&encode(&list)?)?, list);
		assert_eq!(decode(&encode(&[])?)?, Vec::<Occurrence>::new());
		Ok(())
	}

	#[test]
	fn the_log_appends_after_a_reload_and_serializes_like_a_vec() -> Result<()> {
		let a = info("a");
		let p = PatternID::new();
		let list: Vec<Occurrence> = (0..50).map(|i| occ(&a, p, 1_700_000_000_000 + i * 60_000, 600_000, 10)).collect();
		let mut log = OccurrenceLog::decode(&encode(&list[..30])?)?;
		for o in &list[30..] {
			log.push(o);
		}
		assert_eq!(log, list.iter().cloned().collect::<OccurrenceLog>());
		assert_eq!(log.as_vec(), &list);
		assert_eq!(log.first().as_ref(), list.first());
		assert_eq!(serde_json::to_string(&log)?, serde_json::to_string(&list)?);
		assert_eq!(serde_json::from_str::<OccurrenceLog>(&serde_json::to_string(&list)?)?, log);
		Ok(())
	}

	#[test]
	fn evenly_spaced_occurrences_cost_about_three_bytes_each() -> Result<()> {
		let a = info("a");
		let p = PatternID::new();
		let list: Vec<Occurrence> = (0..10_000).map(|i| occ(&a, p, 1_700_000_000_000 + i * 60_000, 600_000, 10)).collect();
		let bytes = encode(&list)?;
		let header = serde_json::to_vec(&(list[0].aspect(), list[0].resolution(), list[0].size(), list[0].database_info(), list[0].pattern_id()))?.len();
		assert!(bytes.len() < header + 3 * list.len() + 32, "{} bytes for {} occurrences", bytes.len(), list.len());
		Ok(())
	}

	#[test]
	fn rejects_truncated_and_foreign_payloads() -> Result<()> {
		let a = info("a");
		let bytes = encode(&[occ(&a, PatternID::new(), 1000, 10, 1)])?;
		assert!(decode(&bytes[..bytes.len() - 1]).is_err());
		assert!(decode(&[9]).is_err());
		assert!(decode(&[]).is_err());
		let mut trailing = bytes;
		trailing.push(0);
		assert!(decode(&trailing).is_err());
		Ok(())
	}
}
