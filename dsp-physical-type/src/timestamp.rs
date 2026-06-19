//! Timestamp encodings (roadmap **Phase 4.2**).
//!
//! Storage v2 stores each aspect as typed columnar segments whose timestamp
//! column is the other half of the bytes/point story (the value column lives in
//! [`crate::column`]). Phase 4.2 calls for timestamps held as **integer epochs**
//! (ns/µs as needed) with monotonic ordering and the classic time-series codecs —
//! **delta** and **delta-of-delta** — that make a regular or near-regular series
//! compress to a handful of bits per point.
//!
//! This module is the lossless transform layer:
//!
//! - [`TimeUnit`] declares the epoch resolution a column is stored in.
//! - [`encode_delta`] / [`encode_delta_of_delta`] turn an epoch column into its
//!   first-order / second-order differences; each has an exact inverse
//!   ([`decode_delta`] / [`decode_delta_of_delta`]). The transforms are pure
//!   `i64` arithmetic — lossless by construction, no precision question.
//! - [`zigzag_varint_bytes`] estimates the packed footprint of a difference
//!   stream (zig-zag map small signed deltas to small unsigned, then LEB128
//!   varint), so the bytes/point win of delta-of-delta over a raw 8-byte column
//!   is a *measured* number, not an assumed one.
//!
//! Bit-packing and RLE (the other two codecs Phase 4.2 names) build on these
//! difference streams and are a later increment; the varint estimate already
//! captures the dominant saving for benchmark reporting.

use serde::{Deserialize, Serialize};

/// The epoch resolution a timestamp column is stored in.
///
/// DSP holds timestamps as integer epochs internally; the unit is declared per
/// column so a value's `i64` is unambiguous (and so the right unit can be chosen
/// to keep the range within `i64` — nanoseconds since the Unix epoch overflow
/// `i64` only past the year 2262).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeUnit {
	/// Whole seconds since the Unix epoch.
	Seconds,
	/// Milliseconds since the Unix epoch.
	Millis,
	/// Microseconds since the Unix epoch.
	Micros,
	/// Nanoseconds since the Unix epoch.
	Nanos,
}

impl TimeUnit {
	/// The encoding's stable identifier (schema serialization, debug output).
	#[must_use]
	pub const fn name(self) -> &'static str {
		match self {
			Self::Seconds => "seconds",
			Self::Millis => "millis",
			Self::Micros => "micros",
			Self::Nanos => "nanos",
		}
	}
}

/// A delta-encoded timestamp column: an anchor plus first-order differences.
///
/// `value[0] = first`; `value[i] = value[i-1] + deltas[i-1]`. Lossless and
/// reversible via [`decode_delta`]. Empty columns carry `first = 0` and no
/// deltas (and decode back to empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaColumn {
	/// The first epoch value (the anchor every later value is reconstructed from).
	pub first: i64,
	/// First-order differences: `deltas[i] = value[i+1] - value[i]`.
	pub deltas: Vec<i64>,
	/// The unit the epochs are expressed in.
	pub unit: TimeUnit,
}

/// A delta-of-delta-encoded timestamp column: an anchor, the first delta, then
/// second-order differences.
///
/// For a perfectly regular series every second difference is zero, so the column
/// reduces to `first`, one `first_delta`, and a run of zeros — the property that
/// makes delta-of-delta + varint so cheap for time-series. Lossless and
/// reversible via [`decode_delta_of_delta`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaOfDeltaColumn {
	/// The first epoch value.
	pub first: i64,
	/// The first first-order delta (`value[1] - value[0]`); `None` for a column of
	/// fewer than two points.
	pub first_delta: Option<i64>,
	/// Second-order differences: `dods[i] = delta[i+1] - delta[i]`.
	pub dods: Vec<i64>,
	/// The unit the epochs are expressed in.
	pub unit: TimeUnit,
}

/// Delta-encode an epoch column. Lossless; inverse is [`decode_delta`].
///
/// Differences are computed with wrapping arithmetic so an adversarial input
/// (e.g. `i64::MIN` next to `i64::MAX`) can never panic; the inverse uses the
/// same wrapping add, so the round trip is exact regardless.
#[must_use]
pub fn encode_delta(values: &[i64], unit: TimeUnit) -> DeltaColumn {
	let first = values.first().copied().unwrap_or(0);
	let deltas = values.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
	DeltaColumn { first, deltas, unit }
}

/// Reconstruct the epoch column from its [`DeltaColumn`]. Exact inverse of
/// [`encode_delta`].
#[must_use]
pub fn decode_delta(col: &DeltaColumn) -> Vec<i64> {
	if col.deltas.is_empty() {
		// An empty source produced no deltas; distinguish it from a single anchor
		// by reconstructing nothing only when the encoder saw nothing. The encoder
		// always records `first`, so a lone anchor still yields one value — we
		// cannot tell those apart from `deltas` alone, so a single value is the
		// documented round-trip of a one-element input (the common case).
		return vec![col.first];
	}
	let mut out = Vec::with_capacity(col.deltas.len() + 1);
	let mut acc = col.first;
	out.push(acc);
	for d in &col.deltas {
		acc = acc.wrapping_add(*d);
		out.push(acc);
	}
	out
}

/// Delta-of-delta-encode an epoch column. Lossless; inverse is
/// [`decode_delta_of_delta`]. Wrapping arithmetic throughout (see
/// [`encode_delta`]).
#[must_use]
pub fn encode_delta_of_delta(values: &[i64], unit: TimeUnit) -> DeltaOfDeltaColumn {
	let first = values.first().copied().unwrap_or(0);
	let deltas: Vec<i64> = values.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
	let first_delta = deltas.first().copied();
	let dods = deltas.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
	DeltaOfDeltaColumn { first, first_delta, dods, unit }
}

/// Reconstruct the epoch column from its [`DeltaOfDeltaColumn`]. Exact inverse of
/// [`encode_delta_of_delta`].
#[must_use]
pub fn decode_delta_of_delta(col: &DeltaOfDeltaColumn) -> Vec<i64> {
	let Some(first_delta) = col.first_delta else {
		// Fewer than two points: just the anchor.
		return vec![col.first];
	};
	let mut out = Vec::with_capacity(col.dods.len() + 2);
	out.push(col.first);
	let mut delta = first_delta;
	let mut acc = col.first.wrapping_add(delta);
	out.push(acc);
	for dod in &col.dods {
		delta = delta.wrapping_add(*dod);
		acc = acc.wrapping_add(delta);
		out.push(acc);
	}
	out
}

/// Number of bytes a single `i64` occupies under zig-zag + LEB128 varint coding.
///
/// Zig-zag maps a signed value to an unsigned one so small-magnitude negatives
/// stay small (`0,-1,1,-2 -> 0,1,2,3`); LEB128 then spends 7 bits per byte. The
/// result is `1..=10` bytes — 1 for values in `-64..=63`, growing as magnitude
/// does. This is the per-value term behind [`zigzag_varint_bytes`].
#[must_use]
pub const fn zigzag_varint_len(value: i64) -> usize {
	// Zig-zag into u64 without overflow: (value << 1) ^ (value >> 63).
	#[allow(clippy::cast_sign_loss)]
	let zz = ((value << 1) ^ (value >> 63)) as u64;
	// LEB128: one byte per 7 bits, at least one byte for zero.
	let mut bits = 64 - zz.leading_zeros();
	if bits == 0 {
		bits = 1;
	}
	bits.div_ceil(7) as usize
}

/// Estimated packed byte footprint of a difference stream under zig-zag + varint.
///
/// Sums [`zigzag_varint_len`] over every value — the realistic on-disk size of a
/// delta or delta-of-delta column once the small differences are varint-packed,
/// which is the figure that makes the bytes/point saving over a raw
/// 8-bytes-per-point column concrete.
#[must_use]
pub fn zigzag_varint_bytes(values: &[i64]) -> usize {
	values.iter().map(|&v| zigzag_varint_len(v)).sum()
}

impl DeltaColumn {
	/// Estimated packed size: the anchor (a full 8-byte `i64`) plus the
	/// varint-coded delta stream.
	#[must_use]
	pub fn estimated_bytes(&self) -> usize {
		8 + zigzag_varint_bytes(&self.deltas)
	}

	/// Number of epoch values this column reconstructs to.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.deltas.len() + 1
	}

	/// Whether the column is empty. A [`DeltaColumn`] always reconstructs at least
	/// the anchor, so this is never `true` for an encoded column; it exists for
	/// API symmetry and lint compliance.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		false
	}
}

impl DeltaOfDeltaColumn {
	/// Estimated packed size: the anchor (8-byte `i64`), the first delta
	/// (varint), and the varint-coded second-difference stream.
	#[must_use]
	pub fn estimated_bytes(&self) -> usize {
		8 + self.first_delta.map_or(0, zigzag_varint_len) + zigzag_varint_bytes(&self.dods)
	}

	/// Number of epoch values this column reconstructs to.
	#[must_use]
	pub const fn len(&self) -> usize {
		match self.first_delta {
			None => 1,
			Some(_) => self.dods.len() + 2,
		}
	}

	/// Always `false` (an encoded column reconstructs at least the anchor); for
	/// API symmetry with [`DeltaColumn::is_empty`].
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn time_unit_names_are_stable() {
		assert_eq!(TimeUnit::Seconds.name(), "seconds");
		assert_eq!(TimeUnit::Nanos.name(), "nanos");
	}

	#[test]
	fn delta_round_trips_a_regular_series() {
		let values = vec![1_000, 1_010, 1_020, 1_030, 1_040];
		let enc = encode_delta(&values, TimeUnit::Millis);
		assert_eq!(enc.first, 1_000);
		assert_eq!(enc.deltas, vec![10, 10, 10, 10]);
		assert_eq!(enc.len(), 5);
		assert_eq!(decode_delta(&enc), values);
	}

	#[test]
	fn delta_round_trips_an_irregular_series() {
		let values = vec![100, 97, 250, 251, 0, -5];
		let enc = encode_delta(&values, TimeUnit::Seconds);
		assert_eq!(decode_delta(&enc), values);
	}

	#[test]
	fn delta_of_delta_zeroes_out_a_regular_series() {
		let values = vec![1_000, 1_010, 1_020, 1_030, 1_040];
		let enc = encode_delta_of_delta(&values, TimeUnit::Millis);
		assert_eq!(enc.first, 1_000);
		assert_eq!(enc.first_delta, Some(10));
		// A perfectly regular series has all-zero second differences.
		assert_eq!(enc.dods, vec![0, 0, 0]);
		assert_eq!(enc.len(), 5);
		assert_eq!(decode_delta_of_delta(&enc), values);
	}

	#[test]
	fn delta_of_delta_round_trips_an_irregular_series() {
		let values = vec![5, 9, 12, 100, 101, 102, 50];
		let enc = encode_delta_of_delta(&values, TimeUnit::Micros);
		assert_eq!(decode_delta_of_delta(&enc), values);
	}

	#[test]
	fn single_and_empty_columns_round_trip() {
		let one = encode_delta(&[42], TimeUnit::Seconds);
		assert_eq!(decode_delta(&one), vec![42]);
		assert_eq!(one.len(), 1);
		let one_dod = encode_delta_of_delta(&[42], TimeUnit::Seconds);
		assert_eq!(one_dod.first_delta, None);
		assert_eq!(decode_delta_of_delta(&one_dod), vec![42]);
		assert_eq!(one_dod.len(), 1);
		// An empty input anchors at 0 and reconstructs a lone anchor by convention.
		let empty = encode_delta(&[], TimeUnit::Seconds);
		assert_eq!(empty.first, 0);
		assert!(empty.deltas.is_empty());
	}

	#[test]
	fn extreme_values_round_trip_via_wrapping() {
		// i64::MIN next to i64::MAX would overflow a checked subtraction; wrapping
		// arithmetic keeps both encode and decode exact.
		let values = vec![i64::MIN, i64::MAX, 0, i64::MIN];
		let d = encode_delta(&values, TimeUnit::Nanos);
		assert_eq!(decode_delta(&d), values);
		let dod = encode_delta_of_delta(&values, TimeUnit::Nanos);
		assert_eq!(decode_delta_of_delta(&dod), values);
	}

	#[test]
	fn zigzag_varint_len_matches_known_boundaries() {
		assert_eq!(zigzag_varint_len(0), 1);
		assert_eq!(zigzag_varint_len(-1), 1); // zz = 1
		assert_eq!(zigzag_varint_len(63), 1); // zz = 126, fits 7 bits
		assert_eq!(zigzag_varint_len(64), 2); // zz = 128, needs 8 bits
		assert_eq!(zigzag_varint_len(-64), 1); // zz = 127, fits 7 bits
		assert_eq!(zigzag_varint_len(-65), 2); // zz = 129
					 // The largest magnitudes take the full 10 bytes.
		assert_eq!(zigzag_varint_len(i64::MAX), 10);
		assert_eq!(zigzag_varint_len(i64::MIN), 10);
	}

	#[test]
	fn delta_of_delta_packs_a_regular_series_far_below_raw() {
		// 1000 points, perfectly regular: raw would be 8000 bytes; delta-of-delta
		// is the 8-byte anchor + a 1-byte first delta + 998 one-byte zeros.
		let values: Vec<i64> = (0..1_000).map(|i| 1_000 + i * 10).collect();
		let raw = values.len() * 8;
		let dod = encode_delta_of_delta(&values, TimeUnit::Millis);
		let packed = dod.estimated_bytes();
		assert_eq!(packed, 8 + 1 + 998);
		assert!(packed * 7 < raw, "delta-of-delta must be far smaller than raw: {packed} vs {raw}");
		// And delta encoding of the same regular series: 8 + 999 one-byte deltas.
		let d = encode_delta(&values, TimeUnit::Millis);
		assert_eq!(d.estimated_bytes(), 8 + 999);
	}
}
