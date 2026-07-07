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

/// The first out-of-order position in an epoch column, if any.
///
/// Scans for the first index `i` where `timestamps[i] < timestamps[i-1]` — a
/// genuine backwards step that breaks monotonic **non-decreasing** order — and
/// returns `(i, previous, current)` for it. Equal neighbours are in order (a
/// duplicate timestamp is not a violation, matching
/// [`SegmentStats::time_sorted`](crate::segment::SegmentStats::time_sorted), which
/// admits `ts[i] == ts[i-1]`). Returns [`None`] for an empty, single-row, or fully
/// ordered column.
///
/// This is the primitive behind the order-enforcing build/seal paths
/// ([`Segment::build_sorted`](crate::segment::Segment::build_sorted),
/// [`AspectSchema::seal_sorted`](crate::schema::AspectSchema::seal_sorted)): they
/// reject a batch at its first backwards step instead of silently storing it with
/// `time_sorted = false`.
#[must_use]
pub fn first_order_violation(timestamps: &[i64]) -> Option<(usize, i64, i64)> {
	timestamps.windows(2).position(|w| w[1] < w[0]).map(|i| (i + 1, timestamps[i], timestamps[i + 1]))
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

/// Number of bytes an unsigned `u64` occupies under plain LEB128 varint coding
/// (`1..=10`). Used for run lengths, which are never negative so need no zig-zag.
#[must_use]
pub const fn uvarint_len(value: u64) -> usize {
	let mut bits = 64 - value.leading_zeros();
	if bits == 0 {
		bits = 1;
	}
	bits.div_ceil(7) as usize
}

/// Run-length-encode a value stream into `(value, run_length)` pairs.
///
/// The companion codec to delta-of-delta: a regular series' second differences
/// are a long run of zeros, which RLE collapses to a single `(0, n)` pair. Exact
/// inverse is [`rle_decode`]. An empty input yields no runs.
#[must_use]
pub fn rle_encode(values: &[i64]) -> Vec<(i64, usize)> {
	let mut runs: Vec<(i64, usize)> = Vec::new();
	for &v in values {
		match runs.last_mut() {
			Some((val, count)) if *val == v => *count += 1,
			_ => runs.push((v, 1)),
		}
	}
	runs
}

/// Reconstruct the value stream from its run-length encoding. Exact inverse of
/// [`rle_encode`].
#[must_use]
pub fn rle_decode(runs: &[(i64, usize)]) -> Vec<i64> {
	let mut out = Vec::with_capacity(runs.iter().map(|(_, c)| *c).sum());
	for &(value, count) in runs {
		out.extend(std::iter::repeat_n(value, count));
	}
	out
}

/// Estimated packed footprint of an RLE stream.
///
/// Each run costs a zig-zag-varint value plus an unsigned-varint run length. For
/// a long zero run this is a few bytes regardless of length — the saving
/// delta-of-delta sets up and RLE realizes.
#[must_use]
pub fn rle_varint_bytes(runs: &[(i64, usize)]) -> usize {
	#[allow(clippy::cast_possible_truncation)]
	runs.iter().map(|&(value, count)| zigzag_varint_len(value) + uvarint_len(count as u64)).sum()
}

/// Zig-zag a signed `i64` into an unsigned `u64` (`0,-1,1,-2 -> 0,1,2,3`), so
/// small-magnitude negatives stay numerically small. Inverse of [`unzigzag`].
#[must_use]
#[allow(clippy::cast_sign_loss)] // the bit pattern is exactly the zig-zag mapping.
const fn zigzag(value: i64) -> u64 {
	((value << 1) ^ (value >> 63)) as u64
}

/// Invert [`zigzag`], recovering the original signed `i64` from its zig-zag code.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // `z >> 1` fits `i64`; the XOR restores the sign.
const fn unzigzag(z: u64) -> i64 {
	((z >> 1) as i64) ^ -((z & 1) as i64)
}

/// The fixed bit width needed to hold every value of a difference stream once
/// zig-zag-coded — i.e. the bits of the largest-magnitude second difference.
///
/// `0` for an empty or all-zero stream (nothing but zeros to store); at most
/// `64`. This is the width [`bitpack_encode`] packs every value to, and the term
/// that makes bit-packing beat varint's one-byte-per-value floor for a regular
/// or small-jitter series (differences of a few bits each).
#[must_use]
pub fn bitpack_width(values: &[i64]) -> u32 {
	values.iter().map(|&v| 64 - zigzag(v).leading_zeros()).max().unwrap_or(0)
}

/// Estimated packed footprint of a difference stream under fixed-width
/// bit-packing: a one-byte width header plus `ceil(n * width / 8)` data bytes.
///
/// Like the plain-varint and RLE estimates, this omits the externally-known row
/// count (the segment stats header carries it), so the three are comparable. For
/// a stream of `n` differences each fitting in `w` bits this is `1 + n*w/8`
/// bytes — well under varint's `n`-byte floor whenever `w < 8`.
#[must_use]
pub fn bitpack_bytes(values: &[i64]) -> usize {
	let width = bitpack_width(values) as usize;
	1 + (values.len() * width).div_ceil(8)
}

/// Fixed-width bit-pack a difference stream.
///
/// Returns the chosen `width` (bits per value, from [`bitpack_width`]) and the
/// packed byte buffer (values written LSB-first, back to back). A `width` of `0`
/// packs an all-zero stream to no data bytes. Exact inverse is [`bitpack_decode`].
#[must_use]
pub fn bitpack_encode(values: &[i64]) -> (u32, Vec<u8>) {
	let width = bitpack_width(values);
	if width == 0 {
		return (0, Vec::new());
	}
	let w = width as usize;
	let mut out = vec![0_u8; (values.len() * w).div_ceil(8)];
	let mut bit = 0_usize;
	for &v in values {
		let zz = zigzag(v);
		for b in 0..w {
			if (zz >> b) & 1 == 1 {
				out[(bit + b) / 8] |= 1 << ((bit + b) % 8);
			}
		}
		bit += w;
	}
	(width, out)
}

/// Reconstruct `count` differences from a fixed-width bit-packed buffer. Exact
/// inverse of [`bitpack_encode`]; a `width` of `0` yields `count` zeros.
#[must_use]
pub fn bitpack_decode(width: u32, bytes: &[u8], count: usize) -> Vec<i64> {
	if width == 0 {
		return vec![0; count];
	}
	let w = width as usize;
	let mut out = Vec::with_capacity(count);
	let mut bit = 0_usize;
	for _ in 0..count {
		let mut zz = 0_u64;
		for b in 0..w {
			let idx = bit + b;
			if bytes.get(idx / 8).is_some_and(|byte| (byte >> (idx % 8)) & 1 == 1) {
				zz |= 1 << b;
			}
		}
		out.push(unzigzag(zz));
		bit += w;
	}
	out
}

/// The fixed block size the realized dynamic bit-pack codec partitions a
/// second-difference stream into.
///
/// 64 amortizes the one-byte per-block width header (≈1.6% overhead on a full block)
/// while still adapting width at a page-ish granularity, so a contiguous wide region
/// is confined to the few blocks it spans instead of widening the whole column. The
/// value rides in the `.dspseg` block (a uvarint), so it can change without breaking
/// old frames.
pub const BLOCKED_BITPACK_BLOCK: usize = 64;

/// Estimated footprint of a **per-block adaptive** (dynamic) bit-packing of a
/// difference stream.
///
/// The values are split into fixed-size blocks of `block`, and each block is packed
/// at *its own* width, so a low-magnitude run does not pay for a distant wide spike
/// the way a single global [`bitpack_bytes`] width does. Each block costs a one-byte
/// width header plus `ceil(len * width / 8)` data bytes; the block size is fixed (only
/// the final block is short, derivable from the count), so no per-block length is stored.
///
/// Roadmap Phase 6.1 "dynamic bit packing" — realized on disk (the `.dspseg` timestamp
/// block carries a blocked codec option, [`blocked_bitpack_encode`]) and folded into
/// [`DeltaOfDeltaColumn::best_estimated_bytes`]. The trade is `num_blocks - 1` extra
/// width-header bytes in exchange for narrower data on every block that does not
/// contain the widest value. The global-width [`bitpack_bytes`] is the `block >= len`
/// case; comparable with [`bitpack_bytes`] / [`gorilla_bytes`] / [`zigzag_varint_bytes`]
/// (all omit the externally-known row count and the block's self-describing prefix).
/// *(src: "Dynamic Bit Packing", Sensors 2023 — <https://www.mdpi.com/1424-8220/23/20/8575>)*
#[must_use]
pub fn blocked_bitpack_bytes(values: &[i64], block: usize) -> usize {
	if values.is_empty() {
		return 0;
	}
	let block = block.max(1);
	values.chunks(block).map(|chunk| 1 + (chunk.len() * bitpack_width(chunk) as usize).div_ceil(8)).sum()
}

/// Per-block adaptive bit-pack encode of a difference stream: the concatenation, block
/// by block, of a one-byte width header and that block's [`bitpack_encode`] payload.
///
/// The emitted length is exactly the [`blocked_bitpack_bytes`] estimate for the same
/// `(values, block)`. Exact inverse is [`blocked_bitpack_decode`] given the same
/// `block` and value count. An empty input yields an empty buffer.
#[must_use]
pub fn blocked_bitpack_encode(values: &[i64], block: usize) -> Vec<u8> {
	let block = block.max(1);
	let mut out = Vec::new();
	for chunk in values.chunks(block) {
		let (width, packed) = bitpack_encode(chunk);
		// Width is 0..=64 by construction, so the conversion never saturates.
		out.push(u8::try_from(width).unwrap_or(64));
		out.extend_from_slice(&packed);
	}
	out
}

/// Reconstruct `count` differences from a per-block adaptive bit-pack buffer.
///
/// Exact inverse of [`blocked_bitpack_encode`] given the same `block` and `count`;
/// bytes past the buffer read as `0` (a truncated block yields zeros rather than
/// panicking).
#[must_use]
pub fn blocked_bitpack_decode(bytes: &[u8], block: usize, count: usize) -> Vec<i64> {
	let block = block.max(1);
	let mut out = Vec::with_capacity(count);
	let mut pos = 0_usize;
	let mut remaining = count;
	while remaining > 0 {
		let block_len = remaining.min(block);
		let width = u32::from(bytes.get(pos).copied().unwrap_or(0));
		pos += 1;
		let data_len = (block_len * width as usize).div_ceil(8);
		let chunk = bytes.get(pos..pos + data_len).unwrap_or(&[]);
		pos += data_len;
		out.extend(bitpack_decode(width, chunk, block_len));
		remaining -= block_len;
	}
	out
}

/// Bit cost of one second-difference value under a **Gorilla-style variable-length**
/// scheme (roadmap Phase 6.1 — evaluation of a per-value bucketed codec as a
/// complement to fixed-width bit-packing).
///
/// Where [`bitpack_width`] pays the max width for *every* value in a block, Gorilla's
/// timestamp scheme pays per value by magnitude bucket, so the common regular case (a
/// zero delta-of-delta) costs a single bit and only genuine jitter pays more:
///
/// - `0` → 1 bit (control `0`) — the regular-interval case;
/// - `[-63, 64]` → 9 bits (control `10` + 7);
/// - `[-255, 256]` → 12 bits (control `110` + 9);
/// - `[-2047, 2048]` → 16 bits (control `1110` + 12);
/// - otherwise → 68 bits (control `1111` + a full 64-bit value).
///
/// The Gorilla paper's final bucket is 32 bits (it assumes deltas fit 32 bits); this
/// uses 64 so the estimate stays a valid upper bound for an arbitrary `i64` timestamp
/// delta. Estimation only — no bitstream is produced. *(src: Gorilla, VLDB'15 —
/// bucketed delta-of-delta timestamp coding.)*
#[must_use]
pub const fn gorilla_dod_bits(dod: i64) -> usize {
	if dod == 0 {
		1
	} else if dod >= -63 && dod <= 64 {
		2 + 7
	} else if dod >= -255 && dod <= 256 {
		3 + 9
	} else if dod >= -2047 && dod <= 2048 {
		4 + 12
	} else {
		4 + 64
	}
}

/// Estimated byte footprint of a second-difference stream under the Gorilla-style
/// variable-length scheme: the summed [`gorilla_dod_bits`] over every value, rounded
/// up to whole bytes.
///
/// Comparable with [`bitpack_bytes`] / [`zigzag_varint_bytes`] / [`rle_varint_bytes`]
/// (all omit the externally-known row count). The win over fixed-width bit-packing
/// shows on a **small-jitter** stream — mostly-zero second differences with rare large
/// spikes — where bit-packing must widen every value to the spike's width while
/// Gorilla pays one bit for each of the many zeros; on a *perfectly* regular stream
/// bit-packing's zero-width case (one header byte, no data) still wins.
#[must_use]
pub fn gorilla_bytes(dods: &[i64]) -> usize {
	let bits: usize = dods.iter().map(|&d| gorilla_dod_bits(d)).sum();
	bits.div_ceil(8)
}

/// Write `n` low bits of `value` LSB-first at bit cursor `*bit` into a pre-sized
/// buffer, advancing the cursor. Same bit order as [`bitpack_encode`], so the two
/// codecs share a decode convention.
fn gorilla_put_bits(out: &mut [u8], bit: &mut usize, value: u64, n: usize) {
	for b in 0..n {
		if (value >> b) & 1 == 1 {
			out[(*bit + b) / 8] |= 1 << ((*bit + b) % 8);
		}
	}
	*bit += n;
}

/// Read `n` bits LSB-first from `bytes` at bit cursor `*bit`, advancing the cursor.
/// Bits past the buffer read as `0` (the exact inverse of [`gorilla_put_bits`] over a
/// buffer sized to the written bit count). Inverse convention of [`bitpack_decode`].
fn gorilla_get_bits(bytes: &[u8], bit: &mut usize, n: usize) -> u64 {
	let mut v = 0_u64;
	for b in 0..n {
		let idx = *bit + b;
		if bytes.get(idx / 8).is_some_and(|byte| (byte >> (idx % 8)) & 1 == 1) {
			v |= 1 << b;
		}
	}
	*bit += n;
	v
}

/// Encode a second-difference stream under the **Gorilla-style variable-length**
/// timestamp codec (roadmap Phase 6.1) — the realized inverse of the advisory
/// [`gorilla_bytes`] estimate.
///
/// Each value is written LSB-first as a unary control prefix naming its magnitude
/// bucket followed by an offset-binary payload of the bucket's width, exactly matching
/// the bit budget [`gorilla_dod_bits`] charges:
///
/// - `0` (1 bit) — a zero second difference (the regular-interval case);
/// - `10` + 7-bit offset for `[-63, 64]`;
/// - `110` + 9-bit offset for `[-255, 256]`;
/// - `1110` + 12-bit offset for `[-2047, 2048]`;
/// - `1111` + a full 64-bit two's-complement word otherwise.
///
/// The offset payload is `dod + (2^(w-1) - 1)` in `w` bits (offset binary), which maps
/// each bucket's `2^w` values onto `0..2^w` losslessly. The emitted length equals the
/// [`gorilla_bytes`] estimate for the stream, so the size the selector weighs is the
/// size actually stored. Exact inverse: [`decode_gorilla_dods`]. *(src: Gorilla,
/// VLDB'15.)*
#[must_use]
#[allow(clippy::cast_sign_loss)] // offset-binary payload is non-negative by construction; the D bucket stores the raw two's-complement bit pattern.
pub fn encode_gorilla_dods(dods: &[i64]) -> Vec<u8> {
	let total_bits: usize = dods.iter().map(|&d| gorilla_dod_bits(d)).sum();
	let mut out = vec![0_u8; total_bits.div_ceil(8)];
	let mut bit = 0_usize;
	for &dod in dods {
		let (ones, width) = gorilla_control(dod);
		for _ in 0..ones {
			gorilla_put_bits(&mut out, &mut bit, 1, 1);
		}
		if ones < 4 {
			gorilla_put_bits(&mut out, &mut bit, 0, 1);
		}
		if width == 64 {
			gorilla_put_bits(&mut out, &mut bit, dod as u64, 64);
		} else if width > 0 {
			let bias = (1_i64 << (width - 1)) - 1;
			gorilla_put_bits(&mut out, &mut bit, (dod + bias) as u64, width as usize);
		}
	}
	out
}

/// The `(unary-ones, payload-width)` control pair for one second difference under the
/// Gorilla codec: `(0, 0)` zero · `(1, 7)` · `(2, 9)` · `(3, 12)` · `(4, 64)`. Kept in
/// lock-step with [`gorilla_dod_bits`]'s bucket boundaries.
const fn gorilla_control(dod: i64) -> (u32, u32) {
	if dod == 0 {
		(0, 0)
	} else if dod >= -63 && dod <= 64 {
		(1, 7)
	} else if dod >= -255 && dod <= 256 {
		(2, 9)
	} else if dod >= -2047 && dod <= 2048 {
		(3, 12)
	} else {
		(4, 64)
	}
}

/// Reconstruct `count` second differences from a [`encode_gorilla_dods`] buffer.
///
/// Reads each value's unary control prefix (consecutive `1`s up to four, terminated by
/// a `0` for the first four classes) then its offset-binary payload, undoing the
/// `+ (2^(w-1) - 1)` bias. Exact inverse of [`encode_gorilla_dods`].
#[must_use]
#[allow(clippy::cast_possible_wrap)] // each payload is < 2^12 (or the D bucket's exact 64-bit round-trip), so the i64 cast never wraps meaningfully.
pub fn decode_gorilla_dods(bytes: &[u8], count: usize) -> Vec<i64> {
	let mut out = Vec::with_capacity(count);
	let mut bit = 0_usize;
	for _ in 0..count {
		let mut ones = 0_u32;
		while ones < 4 && gorilla_get_bits(bytes, &mut bit, 1) == 1 {
			ones += 1;
		}
		let dod = match ones {
			0 => 0,
			1 => gorilla_get_bits(bytes, &mut bit, 7) as i64 - ((1_i64 << 6) - 1),
			2 => gorilla_get_bits(bytes, &mut bit, 9) as i64 - ((1_i64 << 8) - 1),
			3 => gorilla_get_bits(bytes, &mut bit, 12) as i64 - ((1_i64 << 11) - 1),
			_ => gorilla_get_bits(bytes, &mut bit, 64) as i64,
		};
		out.push(dod);
	}
	out
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

	/// Estimated packed size with the second-difference stream **run-length
	/// encoded** instead of plain-varint coded: anchor + first delta + RLE runs.
	///
	/// For a regular or piecewise-regular series the second differences collapse
	/// to a handful of runs, so this is far below [`estimated_bytes`](Self::estimated_bytes);
	/// for a noisy series with few repeats it can be larger (two varints per run),
	/// which is why [`best_estimated_bytes`](Self::best_estimated_bytes) picks the
	/// smaller of the two.
	#[must_use]
	pub fn rle_estimated_bytes(&self) -> usize {
		8 + self.first_delta.map_or(0, zigzag_varint_len) + rle_varint_bytes(&rle_encode(&self.dods))
	}

	/// Estimated packed size with the second-difference stream **fixed-width
	/// bit-packed** instead of varint- or RLE-coded: anchor + first delta +
	/// [`bitpack_bytes`] over the second differences.
	///
	/// This is the cheapest of the three for a regular or small-jitter series,
	/// where every second difference fits in a handful of bits and varint's
	/// one-byte-per-value floor (and RLE's two-varints-per-run) both dominate; a
	/// noisy wide-magnitude stream forces a large width and it loses, which is why
	/// [`best_estimated_bytes`](Self::best_estimated_bytes) takes the minimum.
	#[must_use]
	pub fn bitpack_estimated_bytes(&self) -> usize {
		8 + self.first_delta.map_or(0, zigzag_varint_len) + bitpack_bytes(&self.dods)
	}

	/// Estimated packed size with the second-difference stream coded under the
	/// **Gorilla-style variable-length** scheme ([`gorilla_bytes`]): anchor + first
	/// delta + bucketed-bit stream (roadmap Phase 6.1).
	///
	/// Now **realized on disk** (roadmap Phase 6.1): the `.dspseg` timestamp block
	/// carries a Gorilla codec option ([`encode_gorilla_dods`]) and this estimate is
	/// folded into [`best_estimated_bytes`](Self::best_estimated_bytes) /
	/// [`best_encoding_name`](Self::best_encoding_name), so a segment whose scattered
	/// jitter Gorilla codes smallest stores — and reports — the Gorilla size. Because
	/// [`encode_gorilla_dods`] emits exactly [`gorilla_bytes`] the estimate is the size
	/// on disk (the block's small self-describing length prefix aside), never a codec
	/// the writer cannot produce. Gorilla wins on a small-jitter stream (rare spikes
	/// among mostly-zero second differences, where RLE cannot form runs) and loses on a
	/// perfectly regular one — so it is a *candidate* the min-selector weighs.
	#[must_use]
	pub fn gorilla_estimated_bytes(&self) -> usize {
		8 + self.first_delta.map_or(0, zigzag_varint_len) + gorilla_bytes(&self.dods)
	}

	/// Estimated packed size with the second-difference stream coded under the
	/// **per-block adaptive (dynamic) bit-pack** scheme
	/// ([`blocked_bitpack_bytes`] at [`BLOCKED_BITPACK_BLOCK`]): anchor + first delta +
	/// the per-block stream.
	///
	/// Realized on disk (roadmap Phase 6.1): the `.dspseg` timestamp block carries a
	/// blocked codec option and this estimate is folded into
	/// [`best_estimated_bytes`](Self::best_estimated_bytes) /
	/// [`best_encoding_name`](Self::best_encoding_name). It wins the **mixed-magnitude**
	/// regime — a contiguous wide region among narrow runs — where global bit-packing
	/// widens the whole column, RLE finds no runs, and Gorilla pays its full bucket per
	/// wide value; it ties global bit-packing on a stream of one block (`≤`
	/// [`BLOCKED_BITPACK_BLOCK`]), so the min-selector keeps the simpler `bitpack` label
	/// there.
	#[must_use]
	pub fn blocked_estimated_bytes(&self) -> usize {
		8 + self.first_delta.map_or(0, zigzag_varint_len) + blocked_bitpack_bytes(&self.dods, BLOCKED_BITPACK_BLOCK)
	}

	/// The smallest of the plain-varint, RLE, bit-packed, Gorilla, and per-block adaptive
	/// bit-pack second-difference estimates — the realistic stored size once the cheapest
	/// codec is chosen.
	#[must_use]
	pub fn best_estimated_bytes(&self) -> usize {
		self.estimated_bytes().min(self.rle_estimated_bytes()).min(self.bitpack_estimated_bytes()).min(self.gorilla_estimated_bytes()).min(self.blocked_estimated_bytes())
	}

	/// The stable name of the codec [`best_estimated_bytes`](Self::best_estimated_bytes)
	/// selects: `"delta_of_delta_blocked"` when per-block adaptive bit-packing is the
	/// strict winner (a mixed-magnitude stream — a contiguous wide region among narrow
	/// runs), `"delta_of_delta_gorilla"` when the Gorilla variable-length scheme wins
	/// (scattered single jitter), `"delta_of_delta_bitpack"` when fixed-width bit-packing
	/// wins (a regular or small-jitter series), `"delta_of_delta_rle"` when run-length
	/// coding is cheapest (long identical runs), else `"delta_of_delta"`. The single
	/// source of truth for the codec label so the segment store and the bench report
	/// never disagree on which won — the `.dspseg` writer routes through this name. Ties
	/// favour the simpler codec (plain > rle > bitpack > gorilla > blocked) for a stable
	/// label — in particular a single-block stream ties `bitpack`, which keeps the label.
	#[must_use]
	pub fn best_encoding_name(&self) -> &'static str {
		let plain = self.estimated_bytes();
		let rle = self.rle_estimated_bytes();
		let bitpack = self.bitpack_estimated_bytes();
		let gorilla = self.gorilla_estimated_bytes();
		let blocked = self.blocked_estimated_bytes();
		if blocked < plain && blocked < rle && blocked < bitpack && blocked < gorilla {
			"delta_of_delta_blocked"
		} else if gorilla < plain && gorilla < rle && gorilla < bitpack {
			"delta_of_delta_gorilla"
		} else if bitpack < plain && bitpack < rle {
			"delta_of_delta_bitpack"
		} else if rle < plain {
			"delta_of_delta_rle"
		} else {
			"delta_of_delta"
		}
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
	fn first_order_violation_finds_the_first_backwards_step() {
		// Fully ordered (with a duplicate — equal neighbours are in order) -> None.
		assert_eq!(first_order_violation(&[10, 10, 20, 20, 30]), None);
		// Empty / single-row are vacuously ordered.
		assert_eq!(first_order_violation(&[]), None);
		assert_eq!(first_order_violation(&[42]), None);
		// First backwards step is at row 3 (30 -> 25), reported even though a later
		// pair (50 -> 40) also regresses.
		assert_eq!(first_order_violation(&[10, 20, 30, 25, 50, 40]), Some((3, 30, 25)));
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
	fn rle_round_trips_and_collapses_runs() {
		let values = vec![0, 0, 0, 5, 5, -1, 0, 0];
		let runs = rle_encode(&values);
		assert_eq!(runs, vec![(0, 3), (5, 2), (-1, 1), (0, 2)]);
		assert_eq!(rle_decode(&runs), values);
		// Empty in, empty out.
		assert!(rle_encode(&[]).is_empty());
		assert!(rle_decode(&[]).is_empty());
	}

	#[test]
	fn uvarint_len_matches_known_boundaries() {
		assert_eq!(uvarint_len(0), 1);
		assert_eq!(uvarint_len(127), 1);
		assert_eq!(uvarint_len(128), 2);
		assert_eq!(uvarint_len(u64::MAX), 10);
	}

	#[test]
	fn bitpack_crushes_a_regular_series_second_differences() {
		// 1000 regular points -> 998 zero second-differences.
		let values: Vec<i64> = (0..1_000).map(|i| 1_000 + i * 10).collect();
		let dod = encode_delta_of_delta(&values, TimeUnit::Millis);
		// RLE collapses the zero run to anchor(8) + first_delta(1) + one run
		// (value 0 -> 1 byte, count 998 -> 2 bytes) = 12, itself far below plain
		// varint (8 + 1 + 998).
		assert_eq!(dod.rle_estimated_bytes(), 8 + 1 + (1 + 2));
		assert!(dod.rle_estimated_bytes() < dod.estimated_bytes());
		// But bit-packing an all-zero stream is width 0 -> no data bytes ->
		// anchor(8) + first_delta(1) + width header(1) = 10, one below RLE, so
		// `best` takes bit-packing.
		assert_eq!(dod.bitpack_estimated_bytes(), 8 + 1 + 1);
		assert!(dod.bitpack_estimated_bytes() < dod.rle_estimated_bytes());
		assert_eq!(dod.best_estimated_bytes(), dod.bitpack_estimated_bytes());
	}

	#[test]
	fn gorilla_dod_bits_match_the_paper_buckets() {
		assert_eq!(gorilla_dod_bits(0), 1);
		assert_eq!(gorilla_dod_bits(64), 9);
		assert_eq!(gorilla_dod_bits(-63), 9);
		assert_eq!(gorilla_dod_bits(65), 12);
		assert_eq!(gorilla_dod_bits(256), 12);
		assert_eq!(gorilla_dod_bits(257), 16);
		assert_eq!(gorilla_dod_bits(2048), 16);
		assert_eq!(gorilla_dod_bits(2049), 68);
		assert_eq!(gorilla_dod_bits(-1_000_000), 68);
	}

	#[test]
	fn gorilla_beats_bitpack_on_small_jitter_with_rare_spikes() {
		// 64 mostly-regular points with two large isolated gaps: the second-difference
		// stream is almost all zeros with a few big spikes. Fixed-width bit-packing must
		// widen every value to the spike's width; the Gorilla-style codec pays a single
		// bit for each of the many zero second differences — the roadmap-6.1 hypothesis.
		let mut ts = Vec::with_capacity(64);
		let mut t = 0_i64;
		for i in 0..64 {
			t += 1_000;
			if i == 20 || i == 44 {
				t += 50_000; // an irregular gap
			}
			ts.push(t);
		}
		let dod = encode_delta_of_delta(&ts, TimeUnit::Millis);
		// Measured on this stream: gorilla 52 B vs fixed-width bit-pack 143 B (the
		// roadmap-6.1 comparison). The shipped RLE codec (32 B here) still wins on these
		// *isolated* spikes, though — gorilla's unique advantage is scattered single
		// jitter, where RLE cannot form runs (filed as a ROADMAP follow-up).
		assert!(dod.gorilla_estimated_bytes() < dod.bitpack_estimated_bytes(), "gorilla {} should beat bit-pack {} on a spiky stream", dod.gorilla_estimated_bytes(), dod.bitpack_estimated_bytes());
	}

	#[test]
	fn gorilla_codec_round_trips_every_bucket() {
		// One value drawn from each control class (zero / 7 / 9 / 12 / 64-bit), plus the
		// inclusive bucket boundaries, plus a full-width extreme — the realized codec
		// must be an exact inverse across all of them.
		let dods = vec![0_i64, 1, -1, 64, -63, 65, 256, -255, 257, 2048, -2047, 2049, -2048, i64::MAX, i64::MIN, 1_000_000, -1_000_000];
		let bytes = encode_gorilla_dods(&dods);
		assert_eq!(bytes.len(), gorilla_bytes(&dods), "the realized codec length equals the gorilla_bytes estimate");
		assert_eq!(decode_gorilla_dods(&bytes, dods.len()), dods, "gorilla codec is lossless across every bucket");
	}

	#[test]
	fn gorilla_codec_realized_length_matches_the_estimate_on_real_columns() {
		// The whole point of adopting the codec: the advisory gorilla_estimated_bytes the
		// selector would weigh is the size actually written, not a fiction. Check it over
		// several stream shapes (regular, scattered jitter, spiky, wide-magnitude).
		let regular: Vec<i64> = (0..500).map(|i| 1_000 + i * 10).collect();
		let mut jitter = Vec::with_capacity(500);
		let mut mono = Vec::with_capacity(500);
		let mut t = 0_i64;
		let mut m = 0_i64;
		for i in 0..500 {
			t += if i % 8 == 7 { 2_500 } else { 1_000 };
			jitter.push(t);
			m += 1_000 + (i as i64 % 300);
			mono.push(m);
		}
		for series in [&regular, &jitter, &mono] {
			let dod = encode_delta_of_delta(series, TimeUnit::Millis);
			let realized = encode_gorilla_dods(&dod.dods);
			assert_eq!(realized.len(), gorilla_bytes(&dod.dods), "realized == estimate for the dod stream");
			assert_eq!(decode_gorilla_dods(&realized, dod.dods.len()), dod.dods, "the stream reconstructs exactly");
		}
	}

	#[test]
	fn gorilla_wins_scattered_single_jitter_within_its_bucket() {
		// The roadmap-6.1 predicted win regime: a regular 1000ms base where every 16th
		// interval carries an isolated moderate jitter (fits the +/-2048 bucket). RLE
		// cannot form runs across the scattered spikes and bit-packing must widen every
		// value to the spike's width, so Gorilla — one bit per regular value, a bounded
		// bucket per spike — is the strict winner. Measured here: gorilla 368 B vs the
		// best shipped codec (RLE) 508 B, ~28% smaller. The realized codec matches.
		let mut ts = Vec::with_capacity(1000);
		let mut t = 0_i64;
		for i in 0..1000 {
			t += if i % 16 == 15 { 1_000 + 1_500 } else { 1_000 };
			ts.push(t);
		}
		let dod = encode_delta_of_delta(&ts, TimeUnit::Millis);
		let g = dod.gorilla_estimated_bytes();
		let best_non_gorilla = dod.estimated_bytes().min(dod.rle_estimated_bytes()).min(dod.bitpack_estimated_bytes());
		assert!(g < best_non_gorilla, "gorilla {g} should beat the best non-gorilla codec {best_non_gorilla} in its regime");
		// Now that gorilla is folded in, it IS the overall best here and drives the label.
		assert_eq!(dod.best_estimated_bytes(), g, "gorilla is the folded-in overall best in its regime");
		assert_eq!(dod.best_encoding_name(), "delta_of_delta_gorilla");
		assert_eq!(encode_gorilla_dods(&dod.dods).len(), gorilla_bytes(&dod.dods), "the win is realized, not merely estimated");
	}

	#[test]
	fn gorilla_loses_when_jitter_exceeds_its_widest_bucket() {
		// The honest loss boundary: a jitter of 3000ms produces second differences of
		// +/-3000, past Gorilla's +/-2048 bucket, so each spike falls to the 68-bit
		// fallback and Gorilla blows past varint/RLE/bit-pack. This is why gorilla stays
		// a *candidate* the min-selector weighs, never an unconditional choice.
		let mut ts = Vec::with_capacity(1000);
		let mut t = 0_i64;
		for i in 0..1000 {
			t += if i % 4 == 3 { 1_000 + 3_000 } else { 1_000 };
			ts.push(t);
		}
		let dod = encode_delta_of_delta(&ts, TimeUnit::Millis);
		assert!(dod.gorilla_estimated_bytes() > dod.best_estimated_bytes(), "gorilla must lose past its widest bucket, so the min-selector picks another codec and never routes to gorilla");
		assert_ne!(dod.best_encoding_name(), "delta_of_delta_gorilla", "past its bucket, gorilla is not chosen");
	}

	#[test]
	fn bitpack_still_wins_a_perfectly_regular_stream_over_gorilla() {
		// All-zero second differences: bit-pack's zero-width case (one header byte, no
		// data) beats Gorilla's one-bit-per-value, so Gorilla is a complement, not a
		// replacement — and the realized selector, which excludes the advisory Gorilla
		// estimate, still picks bit-packing.
		let values: Vec<i64> = (0..1_000).map(|i| 1_000 + i * 10).collect();
		let dod = encode_delta_of_delta(&values, TimeUnit::Millis);
		assert!(dod.bitpack_estimated_bytes() <= dod.gorilla_estimated_bytes());
		assert_eq!(dod.best_estimated_bytes(), dod.bitpack_estimated_bytes(), "the gorilla estimate is advisory and does not change the realized selector");
	}

	#[test]
	fn rle_wins_a_long_nonzero_constant_run() {
		// A constant nonzero acceleration: deltas grow by a fixed 5 each step, so the
		// second differences are a long run of 5s. RLE collapses the run to two
		// varints; bit-packing still pays ~4 bits per value, and plain varint one
		// byte per value — so RLE is the strict winner and the chosen label.
		let mut values = vec![0_i64, 1];
		let mut delta = 1_i64;
		for _ in 0..200 {
			delta += 5;
			let next = values.last().unwrap() + delta;
			values.push(next);
		}
		let dod = encode_delta_of_delta(&values, TimeUnit::Seconds);
		assert!(dod.dods.iter().all(|&d| d == 5), "second differences are a constant run of 5");
		assert!(dod.rle_estimated_bytes() < dod.bitpack_estimated_bytes());
		assert!(dod.rle_estimated_bytes() < dod.estimated_bytes());
		assert_eq!(dod.best_encoding_name(), "delta_of_delta_rle");
		assert_eq!(dod.best_estimated_bytes(), dod.rle_estimated_bytes());
	}

	#[test]
	fn best_encoding_name_tracks_the_chosen_codec() {
		// Regular series -> all-zero dods -> bit-packing (width 0) wins -> labelled bitpack.
		let regular: Vec<i64> = (0..1_000).map(|i| 1_000 + i * 10).collect();
		let dod = encode_delta_of_delta(&regular, TimeUnit::Millis);
		assert_eq!(dod.best_encoding_name(), "delta_of_delta_bitpack");
		assert_eq!(dod.best_estimated_bytes(), dod.bitpack_estimated_bytes());
		// A few small distinct second differences plus one wide-magnitude outlier:
		// per-value varint adapts (small values stay 1 byte, the outlier ~5), while a
		// fixed-width bit-pack must widen every value to the outlier's bits and RLE
		// spends two varints per length-1 run -> plain varint wins, labelled plain.
		// dods here are [1, 2, 3, 1_000_000_000].
		let values = vec![0_i64, 1, 3, 7, 14, 21 + 1_000_000_000];
		let dod = encode_delta_of_delta(&values, TimeUnit::Seconds);
		assert_eq!(dod.dods, vec![1, 2, 3, 1_000_000_000]);
		assert!(dod.estimated_bytes() < dod.rle_estimated_bytes());
		assert!(dod.estimated_bytes() < dod.bitpack_estimated_bytes());
		assert_eq!(dod.best_encoding_name(), "delta_of_delta");
		assert_eq!(dod.best_estimated_bytes(), dod.estimated_bytes());
	}

	#[test]
	fn bitpack_round_trips_and_beats_varint_on_small_jitter() {
		// A regular series with small clock jitter: second differences stay within a
		// few bits, so fixed-width bit-packing crushes varint's one-byte floor.
		let dods: Vec<i64> = [0, 1, -1, 2, -2, 1, 0, -1, 1, 0].to_vec();
		let (width, packed) = bitpack_encode(&dods);
		assert_eq!(bitpack_decode(width, &packed, dods.len()), dods, "round trip must be exact");
		// zig-zag of {-2..=2} is {0..=4} -> 3 bits each; 10 values -> ceil(30/8)=4
		// data bytes + 1 width header = 5, vs 10 one-byte varints.
		assert_eq!(width, 3);
		assert_eq!(bitpack_bytes(&dods), 1 + 4);
		assert!(bitpack_bytes(&dods) < zigzag_varint_bytes(&dods));
	}

	#[test]
	fn blocked_bitpack_matches_global_on_a_uniform_stream() {
		// When every value shares one width, per-block packing only adds header bytes,
		// so a single global width is at least as good — the eval must not overclaim.
		let dods: Vec<i64> = (0..128).map(|i| (i % 5) - 2).collect(); // all within 3 bits
		// One block spanning everything == the global bitpack figure exactly.
		assert_eq!(blocked_bitpack_bytes(&dods, dods.len()), bitpack_bytes(&dods));
		assert_eq!(blocked_bitpack_bytes(&dods, 1_000_000), bitpack_bytes(&dods));
		// Splitting a uniform-width stream only adds width-header bytes.
		assert!(blocked_bitpack_bytes(&dods, 16) >= bitpack_bytes(&dods));
	}

	#[test]
	fn blocked_bitpack_beats_every_shipped_codec_on_a_mixed_magnitude_stream() {
		// Phase 6.1 eval finding: a stream with a contiguous WIDE region among otherwise
		// NARROW runs is the regime where per-block adaptive bit-packing wins — global
		// bit-packing must widen every value to the spike width, RLE finds no runs (the
		// values differ), the varint pays multi-byte on every wide value, and Gorilla
		// pays its 68-bit bucket per wide value plus a control prefix on every narrow one.
		let mut dods: Vec<i64> = Vec::new();
		for i in 0..256 {
			// One 32-wide window of large values; the rest is small ±1 jitter.
			dods.push(if (96..128).contains(&i) { 500_000_000 + i64::from(i) } else { i64::from(i % 3) - 1 });
		}
		let block = 32;
		let blocked = blocked_bitpack_bytes(&dods, block);
		let global = bitpack_bytes(&dods);
		let varint = zigzag_varint_bytes(&dods);
		let gorilla = gorilla_bytes(&dods);
		let rle = rle_varint_bytes(&rle_encode(&dods));
		// Adaptive bit-packing is the strict winner in this regime.
		assert!(blocked < global, "blocked {blocked} must beat global bit-pack {global}");
		assert!(blocked < varint, "blocked {blocked} must beat varint {varint}");
		assert!(blocked < gorilla, "blocked {blocked} must beat gorilla {gorilla}");
		assert!(blocked < rle, "blocked {blocked} must beat rle {rle}");
		// The saving over global bit-packing is large (global pays the ~30-bit spike
		// width for all 256 values; blocked pays it only for the one wide block).
		assert!(blocked * 3 < global, "blocked {blocked} should be well under a third of global {global}");
	}

	#[test]
	fn blocked_bitpack_handles_all_zero_and_empty_streams() {
		assert_eq!(blocked_bitpack_bytes(&[], 8), 0);
		// An all-zero stream packs each block to just its width header byte.
		assert_eq!(blocked_bitpack_bytes(&[0; 20], 8), 3); // ceil(20/8) = 3 blocks
		// A zero block size is clamped to 1 (one header byte per value), never a panic.
		assert_eq!(blocked_bitpack_bytes(&[0, 0, 0], 0), 3);
	}

	#[test]
	fn blocked_bitpack_encode_round_trips_and_matches_the_byte_estimate() {
		// Mixed-magnitude stream; encode length must equal blocked_bitpack_bytes and the
		// decode must be exact for several block sizes and awkward tail lengths.
		let vals: Vec<i64> = (0..130).map(|i| if (40..56).contains(&i) { 1_000_000 + i } else { (i % 7) - 3 }).collect();
		for block in [1_usize, 7, 16, 64, 130, 1000] {
			let bytes = blocked_bitpack_encode(&vals, block);
			assert_eq!(bytes.len(), blocked_bitpack_bytes(&vals, block), "encoded length must equal the estimate (block={block})");
			assert_eq!(blocked_bitpack_decode(&bytes, block, vals.len()), vals, "round trip must be exact (block={block})");
		}
		// Empty stream encodes to nothing and decodes to nothing.
		assert!(blocked_bitpack_encode(&[], 64).is_empty());
		assert_eq!(blocked_bitpack_decode(&[], 64, 0), Vec::<i64>::new());
	}

	/// A DoD column whose second differences are mostly narrow ±1 jitter with one
	/// contiguous wide window (blocks 96..128 fall inside a single 64-wide block) — the
	/// mixed-magnitude regime where per-block adaptive bit-packing is the strict winner.
	fn mixed_magnitude_dod_column() -> DeltaOfDeltaColumn {
		let dods: Vec<i64> = (0..256).map(|i| if (96..128).contains(&i) { 500_000_000 + i } else { (i % 3) - 1 }).collect();
		DeltaOfDeltaColumn { first: 1_000, first_delta: Some(1_000), dods, unit: TimeUnit::Millis }
	}

	#[test]
	fn best_encoding_picks_blocked_on_a_mixed_magnitude_stream() {
		// A multi-block stream with a contiguous wide window: per-block adaptive
		// bit-packing is the strict winner, so best_encoding_name selects it and
		// best_estimated_bytes equals the blocked estimate.
		let dod = mixed_magnitude_dod_column();
		assert_eq!(dod.best_encoding_name(), "delta_of_delta_blocked", "mixed-magnitude dods pick the blocked codec");
		assert_eq!(dod.best_estimated_bytes(), dod.blocked_estimated_bytes());
		assert!(dod.blocked_estimated_bytes() < dod.bitpack_estimated_bytes(), "blocked must beat global bit-pack here");
		// A single-block stream ties global bit-packing, which keeps the simpler label.
		let regular: Vec<i64> = (0..40).map(|i| 1_000 + i * 10).collect();
		let small = encode_delta_of_delta(&regular, TimeUnit::Millis);
		assert_ne!(small.best_encoding_name(), "delta_of_delta_blocked", "a single-block stream must not pick blocked");
	}

	#[test]
	fn bitpack_handles_all_zero_and_empty_streams() {
		assert_eq!(bitpack_width(&[]), 0);
		assert_eq!(bitpack_width(&[0, 0, 0]), 0);
		let (w, packed) = bitpack_encode(&[0, 0, 0]);
		assert_eq!(w, 0);
		assert!(packed.is_empty());
		assert_eq!(bitpack_decode(0, &[], 3), vec![0, 0, 0]);
		// An all-zero stream packs to just the width header byte.
		assert_eq!(bitpack_bytes(&[0, 0, 0]), 1);
	}

	#[test]
	fn bitpack_round_trips_a_wide_magnitude_stream() {
		// Large magnitudes force a wide width but must still round-trip exactly.
		let dods = [i64::MIN, -1, 0, 1, i64::MAX, 123_456_789, -987_654_321];
		let (width, packed) = bitpack_encode(&dods);
		assert_eq!(width, 64, "i64::MIN zig-zags to u64::MAX -> 64 bits");
		assert_eq!(bitpack_decode(width, &packed, dods.len()), dods);
	}

	#[test]
	fn best_estimate_falls_back_to_varint_on_a_wide_non_repeating_stream() {
		// Distinct second differences (so RLE spends two varints per length-1 run and
		// loses) plus one wide-magnitude outlier (so a fixed-width bit-pack must widen
		// every value and loses too): per-value varint is the strict winner.
		let values = vec![0_i64, 1, 3, 7, 14, 21 + 1_000_000_000];
		let dod = encode_delta_of_delta(&values, TimeUnit::Seconds);
		assert_eq!(dod.dods, vec![1, 2, 3, 1_000_000_000]);
		assert!(dod.rle_estimated_bytes() > dod.estimated_bytes(), "RLE must lose on a non-repeating stream");
		assert!(dod.bitpack_estimated_bytes() > dod.estimated_bytes(), "bit-packing must lose on a wide-magnitude stream");
		assert_eq!(dod.best_estimated_bytes(), dod.estimated_bytes());
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
