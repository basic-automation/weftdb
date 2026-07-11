//! Gorilla-style XOR compression for `f64` value columns (roadmap Phase 6.1 —
//! the f64 value-column codec, distinct from the timestamp delta-of-delta codecs).
//!
//! The shipped value codecs ([`crate::column::ColumnEncoding`]) all target the
//! **scaled-integer** payload (`ScaledI64` mantissas — bit-pack, per-block adaptive,
//! Frame-of-Reference). An `F64` column has none of those: its `.dspseg` payload is
//! the raw little-endian IEEE-754 byte pattern, eight bytes per value, *uncompressed*.
//! Real lossy-within-tolerance series (a sensor reading the physical-type recommender
//! lands on `F64`) therefore pay the full 8 B/point even when consecutive samples
//! barely move.
//!
//! This module is the missing codec: Facebook's **Gorilla** value compression, which
//! XORs each IEEE bit pattern against its predecessor and stores only the run of
//! *meaningful* bits between the leading and trailing zeros. A slowly-varying series
//! shares most high (sign/exponent) and low (mantissa) bits between neighbours, so the
//! XOR is mostly zeros and only a handful of bits are written per value; a repeated
//! value costs a single bit. It is bit-exact for **every** `f64` — including `NaN`,
//! `±inf`, subnormals, and both signed zeros — because it operates purely on the 64-bit
//! pattern ([`f64::to_bits`]) and never interprets the number.
//!
//! Gorilla is also the *baseline* the roadmap's Chimp / Chimp128 targets improve on
//! (XOR against the best of the previous 128 values rather than only the immediate
//! predecessor). This is the foundation codec + an advisory byte estimate
//! ([`ColumnEncoding::gorilla_f64_bytes`](crate::column::ColumnEncoding::gorilla_f64_bytes));
//! it is **not** yet wired into the `.dspseg` writer or any codec selector (that is the
//! adopt-or-drop slice, exactly as the Gorilla-timestamp and FOR codecs were introduced
//! advisory-first and adopted later). *(src: Gorilla, VLDB'15 —
//! <https://www.vldb.org/pvldb/vol8/p1816-teller.pdf> · Chimp, VLDB'22 —
//! <https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf>)*

/// A minimal MSB-first bit writer over a growing byte buffer.
///
/// Bits are appended most-significant-first within each byte, so a field written with
/// [`put_bits`](Self::put_bits) reads back in the same order via [`BitReader::get_bits`].
/// The final partial byte is zero-padded; the padding is never read because decoding is
/// driven by an externally-known value count, not by buffer length.
struct BitWriter {
	bytes: Vec<u8>,
	/// Number of bits already written into the last byte (`0..8`); `0` means the next
	/// bit starts a fresh byte.
	bit: u32,
}

impl BitWriter {
	const fn new() -> Self {
		Self { bytes: Vec::new(), bit: 0 }
	}

	/// Append a single bit (the low bit of `b`).
	fn put_bit(&mut self, b: u64) {
		if self.bit == 0 {
			self.bytes.push(0);
		}
		if b & 1 == 1 {
			let last = self.bytes.len() - 1;
			// MSB-first: the first bit of a byte lands in bit 7.
			self.bytes[last] |= 1 << (7 - self.bit);
		}
		self.bit = (self.bit + 1) % 8;
	}

	/// Append the low `count` bits of `value`, most-significant first. `count` is `0..=64`.
	fn put_bits(&mut self, value: u64, count: u32) {
		for i in (0..count).rev() {
			self.put_bit(value >> i);
		}
	}

	fn into_bytes(self) -> Vec<u8> {
		self.bytes
	}
}

/// The MSB-first reader paired with [`BitWriter`]. Reads past the end of the buffer
/// yield zero bits (a truncated stream decodes to a well-defined value rather than
/// panicking).
struct BitReader<'a> {
	bytes: &'a [u8],
	/// Absolute bit cursor from the start of the buffer.
	pos: usize,
}

impl<'a> BitReader<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, pos: 0 }
	}

	fn get_bit(&mut self) -> u64 {
		let byte = self.pos / 8;
		let off = u32::try_from(self.pos % 8).unwrap_or(0);
		self.pos += 1;
		self.bytes.get(byte).map_or(0, |b| u64::from((b >> (7 - off)) & 1))
	}

	/// Read `count` bits (most-significant first) as the low bits of a `u64`. `count`
	/// is `0..=64`.
	fn get_bits(&mut self, count: u32) -> u64 {
		let mut out = 0_u64;
		for _ in 0..count {
			out = (out << 1) | self.get_bit();
		}
		out
	}
}

/// Encode an `f64` column with the Gorilla XOR scheme.
///
/// Layout: the first value's 64-bit pattern verbatim, then for each subsequent value the
/// XOR against its predecessor —
///
/// - XOR `== 0` (value unchanged) → a single `0` control bit;
/// - otherwise a `1` control bit, then either
///   - `0` + the meaningful bits packed into the *previous* value's `[leading, trailing]`
///     window (when the new XOR's zero-runs both cover the previous window), or
///   - `1` + a 5-bit leading-zero count (capped at 31) + a 6-bit `(meaningful-1)` length
///     + the `meaningful` significant bits.
///
/// Exact inverse: [`xor_f64_decode`] with the same value count. An empty column yields an
/// empty buffer.
#[must_use]
pub fn xor_f64_encode(values: &[f64]) -> Vec<u8> {
	let mut w = BitWriter::new();
	let Some((&first, rest)) = values.split_first() else {
		return Vec::new();
	};
	let mut prev = first.to_bits();
	w.put_bits(prev, 64);
	// Sentinel: no previous window established yet.
	let mut prev_leading = u32::MAX;
	let mut prev_trailing = 0_u32;
	for &value in rest {
		let bits = value.to_bits();
		let xor = prev ^ bits;
		prev = bits;
		if xor == 0 {
			w.put_bit(0);
			continue;
		}
		w.put_bit(1);
		// Cap leading at 31 so it fits the 5-bit field; trailing is exact.
		let leading = xor.leading_zeros().min(31);
		let trailing = xor.trailing_zeros();
		if prev_leading != u32::MAX && leading >= prev_leading && trailing >= prev_trailing {
			// Reuse the previous window: no leading/length header, just the bits.
			w.put_bit(0);
			let significant = 64 - prev_leading - prev_trailing;
			w.put_bits(xor >> prev_trailing, significant);
		} else {
			w.put_bit(1);
			let significant = 64 - leading - trailing;
			w.put_bits(u64::from(leading), 5);
			// significant is 1..=64; store (significant - 1) in 6 bits (0..=63).
			w.put_bits(u64::from(significant - 1), 6);
			w.put_bits(xor >> trailing, significant);
			prev_leading = leading;
			prev_trailing = trailing;
		}
	}
	w.into_bytes()
}

/// Reconstruct `count` `f64` values from a [`xor_f64_encode`] buffer.
///
/// Bit-exact for every input (the XOR round-trips the raw 64-bit pattern). A `count` of
/// `0` yields an empty vector; a truncated buffer decodes trailing values as if the
/// missing bits were zero rather than panicking.
#[must_use]
pub fn xor_f64_decode(bytes: &[u8], count: usize) -> Vec<f64> {
	if count == 0 {
		return Vec::new();
	}
	let mut r = BitReader::new(bytes);
	let mut prev = r.get_bits(64);
	let mut out = Vec::with_capacity(count);
	out.push(f64::from_bits(prev));
	let mut prev_leading = 0_u32;
	let mut prev_trailing = 0_u32;
	for _ in 1..count {
		if r.get_bit() == 0 {
			// Unchanged value.
			out.push(f64::from_bits(prev));
			continue;
		}
		let (leading, significant) = if r.get_bit() == 0 {
			// Reuse the previous window.
			(prev_leading, 64 - prev_leading - prev_trailing)
		} else {
			let leading = u32::try_from(r.get_bits(5)).unwrap_or(0);
			let significant = u32::try_from(r.get_bits(6)).unwrap_or(0) + 1;
			(leading, significant)
		};
		let trailing = 64 - leading - significant;
		let meaningful = r.get_bits(significant);
		let xor = meaningful << trailing;
		prev ^= xor;
		out.push(f64::from_bits(prev));
		prev_leading = leading;
		prev_trailing = trailing;
	}
	out
}

/// The realized byte footprint of [`xor_f64_encode`] for `values`.
///
/// Computed by encoding (the codec is a single fast pass, so this is the exact stored
/// size, not an approximation). Comparable against the raw `8 * len` an uncompressed
/// `F64` value block occupies.
#[must_use]
pub fn xor_f64_bytes(values: &[f64]) -> usize {
	xor_f64_encode(values).len()
}

/// Chimp's leading-zero representation table: the eight leading-zero counts a 3-bit code
/// can name. A value's actual leading-zero count is rounded *down* to the nearest entry,
/// so the meaningful-bit window it names always contains every set bit.
///
/// This is the key size win over the Gorilla baseline ([`xor_f64_encode`]), which spends
/// a full 5-bit leading-zero field on every new window; Chimp spends 3. *(src: Chimp,
/// VLDB'22 — <https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf>)*
const CHIMP_LEADING_REPR: [u32; 8] = [0, 8, 12, 16, 18, 20, 22, 24];

/// The number of trailing zeros above which Chimp trims the trailing run (flag `01`)
/// rather than reusing the leading window — the paper's threshold of 6.
const CHIMP_TRAILING_THRESHOLD: u32 = 6;

/// Round a leading-zero count *down* to its [`CHIMP_LEADING_REPR`] class, returning the
/// 3-bit code index and the represented count (`<= lead`).
const fn chimp_leading_class(lead: u32) -> (u64, u32) {
	let mut idx = 0_usize;
	let mut i = 0_usize;
	while i < CHIMP_LEADING_REPR.len() {
		if lead >= CHIMP_LEADING_REPR[i] {
			idx = i;
		}
		i += 1;
	}
	(idx as u64, CHIMP_LEADING_REPR[idx])
}

/// Encode an `f64` column with a **Chimp-style** XOR scheme.
///
/// The VLDB'22 refinement of [`xor_f64_encode`] (Gorilla): a 2-bit flag per value, a
/// 3-bit leading-zero *class* (rather than Gorilla's 5-bit exact count), and a
/// trailing-zero threshold.
///
/// Layout: the first value's 64-bit pattern verbatim, then per subsequent value the XOR
/// against its predecessor under one of four 2-bit flags —
///
/// - `00` — XOR `== 0` (value unchanged): nothing more.
/// - `01` — a long trailing-zero run (`> 6`): a 3-bit leading class + a 6-bit significant
///   length + the trimmed significant bits (`xor >> trailing`).
/// - `10` — the leading class equals the previous value's: reuse it, writing only the
///   `64 - leading` low bits (no header).
/// - `11` — a new leading class with a short trailing run: a 3-bit leading class + the
///   `64 - leading` low bits.
///
/// Bit-exact for every `f64` (it XORs the raw [`f64::to_bits`] pattern). Exact inverse:
/// [`chimp_f64_decode`] with the same value count. **Advisory only** (roadmap Phase 6.1),
/// benchmarked head-to-head against the Gorilla baseline; neither is on disk yet (the
/// adopt-or-drop slice picks the winner). *(src: Chimp, VLDB'22 —
/// <https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf>)*
#[must_use]
pub fn chimp_f64_encode(values: &[f64]) -> Vec<u8> {
	let mut w = BitWriter::new();
	let Some((&first, rest)) = values.split_first() else {
		return Vec::new();
	};
	let mut prev = first.to_bits();
	w.put_bits(prev, 64);
	// Sentinel: no leading class established yet, so the first non-zero XOR takes flag `11`.
	let mut stored_leading = u32::MAX;
	for &value in rest {
		let bits = value.to_bits();
		let xor = prev ^ bits;
		prev = bits;
		if xor == 0 {
			w.put_bits(0b00, 2);
			continue;
		}
		let lead_actual = xor.leading_zeros();
		let trailing = xor.trailing_zeros();
		let (lead_code, leading) = chimp_leading_class(lead_actual);
		if trailing > CHIMP_TRAILING_THRESHOLD {
			w.put_bits(0b01, 2);
			w.put_bits(lead_code, 3);
			let significant = 64 - leading - trailing;
			// significant is 1..=57 here (trailing >= 7), so 6 bits hold it directly.
			w.put_bits(u64::from(significant), 6);
			w.put_bits(xor >> trailing, significant);
			stored_leading = leading;
		} else if leading == stored_leading {
			w.put_bits(0b10, 2);
			w.put_bits(xor, 64 - leading);
		} else {
			w.put_bits(0b11, 2);
			w.put_bits(lead_code, 3);
			w.put_bits(xor, 64 - leading);
			stored_leading = leading;
		}
	}
	w.into_bytes()
}

/// Reconstruct `count` `f64` values from a [`chimp_f64_encode`] buffer.
///
/// Bit-exact for every input. A `count` of `0` yields an empty vector; a truncated buffer
/// decodes trailing values as if the missing bits were zero rather than panicking.
#[must_use]
pub fn chimp_f64_decode(bytes: &[u8], count: usize) -> Vec<f64> {
	if count == 0 {
		return Vec::new();
	}
	let mut r = BitReader::new(bytes);
	let mut prev = r.get_bits(64);
	let mut out = Vec::with_capacity(count);
	out.push(f64::from_bits(prev));
	let mut stored_leading = 0_u32;
	for _ in 1..count {
		let flag = r.get_bits(2);
		let xor = match flag {
			0b00 => 0,
			0b01 => {
				let lead_code = usize::try_from(r.get_bits(3)).unwrap_or(0);
				let leading = CHIMP_LEADING_REPR[lead_code & 7];
				let significant = u32::try_from(r.get_bits(6)).unwrap_or(0);
				let trailing = 64 - leading - significant;
				stored_leading = leading;
				r.get_bits(significant) << trailing
			}
			0b10 => r.get_bits(64 - stored_leading),
			_ => {
				let lead_code = usize::try_from(r.get_bits(3)).unwrap_or(0);
				stored_leading = CHIMP_LEADING_REPR[lead_code & 7];
				r.get_bits(64 - stored_leading)
			}
		};
		prev ^= xor;
		out.push(f64::from_bits(prev));
	}
	out
}

/// The realized byte footprint of [`chimp_f64_encode`] for `values`. Comparable against
/// the raw `8 * len` and against the Gorilla baseline [`xor_f64_bytes`].
#[must_use]
pub fn chimp_f64_bytes(values: &[f64]) -> usize {
	chimp_f64_encode(values).len()
}

/// Chimp128's reference-window size — each value XORs against the best of the previous
/// `PREVIOUS_VALUES` samples (not only the immediate predecessor), so a value that revisits
/// an earlier level compresses against *that* level rather than its unrelated neighbour.
const PREVIOUS_VALUES: usize = 128;
/// `log2(PREVIOUS_VALUES)` — the bit width of the ring-slot index a windowed reference writes.
const PREVIOUS_VALUES_LOG2: u32 = 7;
/// The trailing-zero threshold: a windowed reference is taken only when its XOR carries *more*
/// than this many trailing zeros. Set to `6 + log2(PREVIOUS_VALUES)`, so the hash key — the low
/// `THRESHOLD + 1` bits of the value — *guarantees* any hit clears it (two values sharing that
/// many low bits XOR to at least `THRESHOLD + 1` trailing zeros). *(src: Chimp128, VLDB'22 —
/// <https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf>)*
const CHIMP128_THRESHOLD: u32 = 6 + PREVIOUS_VALUES_LOG2;

/// Encode an `f64` column with the **faithful Chimp128** scheme (the 128-value reference
/// window, the roadmap's next f64 slice above single-predecessor [`chimp_f64_encode`]).
///
/// Where Gorilla and depth-1 Chimp XOR each value only against its immediate predecessor,
/// Chimp128 keeps a ring of the previous [`PREVIOUS_VALUES`] samples and a lookup table keyed
/// on the low `CHIMP128_THRESHOLD + 1` bits → the ring index of the most recent value with that
/// low-bit pattern. When that reference's XOR clears [`CHIMP128_THRESHOLD`] trailing zeros the
/// value compresses against it (naming the slot in `log2(128) = 7` bits); otherwise it falls
/// back to the immediate predecessor. This is the win the single-predecessor codecs cannot
/// reach: a signal that oscillates over a small set of levels XORs each sample against the
/// *same* earlier level instead of its unrelated neighbour.
///
/// Four 2-bit flags, packed so the flag falls out of the payload's high bits (no separate flag
/// write for the windowed cases):
/// - `00` — the value equals a windowed reference (XOR `== 0`): a 7-bit ring slot (the top two
///   bits of the 9-bit field are `0`, giving the `00` flag).
/// - `01` — a windowed reference with a long trailing run: `512·(128 + slot) + 64·leadClass +
///   significant` in 18 bits (the `128 + slot` top bit supplies the `01`), then the trimmed
///   `significant` meaningful bits.
/// - `10` — the immediate predecessor, reusing the previous leading class: the `64 - leading`
///   low bits.
/// - `11` — the immediate predecessor, a new leading class: a 3-bit class + the `64 - leading`
///   low bits.
///
/// Bit-exact for every `f64` (it XORs the raw [`f64::to_bits`] pattern — `NaN`, `±inf`,
/// subnormals, both signed zeros). Exact inverse: [`chimp128_f64_decode`] with the same value
/// count. **Advisory only** (roadmap Phase 6.1); benchmarked against Gorilla / depth-1 Chimp
/// before an adopt-or-drop decision realizes a winner on disk. *(src: Chimp128 algorithm —
/// <https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf> · `DuckDB` Chimp128 impl notes —
/// <https://github.com/duckdb/duckdb/pull/4878>)*
#[must_use]
pub fn chimp128_f64_encode(values: &[f64]) -> Vec<u8> {
	let mut w = BitWriter::new();
	let Some((&first, rest)) = values.split_first() else {
		return Vec::new();
	};
	let first_bits = first.to_bits();
	w.put_bits(first_bits, 64);
	let mut ring = [0_u64; PREVIOUS_VALUES];
	ring[0] = first_bits;
	// Hash: low `CHIMP128_THRESHOLD + 1` bits of a value → the absolute index it was last
	// stored at (`usize::MAX` marks an empty slot).
	let mask = (1_u64 << (CHIMP128_THRESHOLD + 1)) - 1;
	let mut indices = vec![usize::MAX; 1_usize << (CHIMP128_THRESHOLD + 1)];
	indices[usize::try_from(first_bits & mask).unwrap_or(0)] = 0;
	// Sentinel: no leading class yet, so the first non-zero XOR cannot take flag `10`.
	let mut stored_leading = u32::MAX;
	// `count` is the absolute index of the value being written (1-based; index 0 is the header).
	for (count, &value) in (1_usize..).zip(rest) {
		let bits = value.to_bits();
		let key = usize::try_from(bits & mask).unwrap_or(0);
		let imm_slot = (count - 1) % PREVIOUS_VALUES;
		// Prefer a windowed reference whose XOR clears the trailing threshold; else the
		// immediate predecessor. The `count - cand < PREVIOUS_VALUES` guard keeps the ring
		// slot unambiguous (the value at `cand % 128` has not been overwritten yet).
		let cand = indices[key];
		let windowed = cand != usize::MAX && count - cand < PREVIOUS_VALUES && (bits ^ ring[cand % PREVIOUS_VALUES]).trailing_zeros() > CHIMP128_THRESHOLD;
		let ref_slot = if windowed { cand % PREVIOUS_VALUES } else { imm_slot };
		let xor = bits ^ ring[ref_slot];
		let trailing = xor.trailing_zeros();
		if xor == 0 {
			// flag `00` + the 7-bit ring slot (top two of the 9-bit field are 0).
			w.put_bits(ref_slot as u64, 2 + PREVIOUS_VALUES_LOG2);
			stored_leading = u32::MAX;
		} else {
			let (lead_code, leading) = chimp_leading_class(xor.leading_zeros());
			if windowed {
				// flag `01`, packed so the leading `01` falls out of `(128 + slot)`'s top bit.
				// `significant` is 1..=50 here (trailing > 13), so 6 bits hold it directly.
				let significant = 64 - leading - trailing;
				let packed = 512 * (PREVIOUS_VALUES as u64 + ref_slot as u64) + 64 * lead_code + u64::from(significant);
				w.put_bits(packed, 2 + PREVIOUS_VALUES_LOG2 + 3 + 6);
				w.put_bits(xor >> trailing, significant);
				stored_leading = u32::MAX;
			} else if leading == stored_leading {
				// flag `10` — reuse the leading class, immediate predecessor.
				w.put_bits(0b10, 2);
				w.put_bits(xor, 64 - leading);
			} else {
				// flag `11` — new leading class, immediate predecessor.
				w.put_bits(0b11, 2);
				w.put_bits(lead_code, 3);
				w.put_bits(xor, 64 - leading);
				stored_leading = leading;
			}
		}
		ring[count % PREVIOUS_VALUES] = bits;
		indices[key] = count;
	}
	w.into_bytes()
}

/// Reconstruct `count` `f64` values from a [`chimp128_f64_encode`] buffer.
///
/// Bit-exact for every input. A `count` of `0` yields an empty vector; a truncated buffer
/// decodes trailing values as if the missing bits were zero rather than panicking. The decoder
/// maintains the identical 128-value ring, so a windowed reference (`00`/`01`) reads the same
/// slot the encoder named.
#[must_use]
pub fn chimp128_f64_decode(bytes: &[u8], count: usize) -> Vec<f64> {
	if count == 0 {
		return Vec::new();
	}
	let mut r = BitReader::new(bytes);
	let first_bits = r.get_bits(64);
	let mut ring = [0_u64; PREVIOUS_VALUES];
	ring[0] = first_bits;
	let mut out = Vec::with_capacity(count);
	out.push(f64::from_bits(first_bits));
	let mut stored_leading = 0_u32;
	// `stored` is the absolute index of the value being decoded (1-based; index 0 is the header).
	for stored in 1..count {
		let flag = r.get_bits(2);
		let bits = match flag {
			0b00 => {
				// Windowed reference, value unchanged: the 7-bit ring slot.
				let slot = usize::try_from(r.get_bits(PREVIOUS_VALUES_LOG2)).unwrap_or(0);
				ring[slot]
			}
			0b01 => {
				// Windowed reference, long trailing run: slot, leading class, significant length.
				let slot = usize::try_from(r.get_bits(PREVIOUS_VALUES_LOG2)).unwrap_or(0);
				let lead_code = usize::try_from(r.get_bits(3)).unwrap_or(0);
				let leading = CHIMP_LEADING_REPR[lead_code & 7];
				let significant = u32::try_from(r.get_bits(6)).unwrap_or(0);
				let trailing = 64 - leading - significant;
				let meaningful = r.get_bits(significant);
				// `stored_leading` is intentionally left unchanged: the encoder resets it to a
				// sentinel after a windowed reference, so it never emits flag `10` (which reads
				// `stored_leading`) before the next flag `11` re-establishes a real class.
				ring[slot] ^ (meaningful << trailing)
			}
			0b10 => {
				// Immediate predecessor, reuse leading class.
				let xor = r.get_bits(64 - stored_leading);
				ring[(stored - 1) % PREVIOUS_VALUES] ^ xor
			}
			_ => {
				// Immediate predecessor, new leading class.
				let lead_code = usize::try_from(r.get_bits(3)).unwrap_or(0);
				stored_leading = CHIMP_LEADING_REPR[lead_code & 7];
				let xor = r.get_bits(64 - stored_leading);
				ring[(stored - 1) % PREVIOUS_VALUES] ^ xor
			}
		};
		ring[stored % PREVIOUS_VALUES] = bits;
		out.push(f64::from_bits(bits));
	}
	out
}

/// The realized byte footprint of [`chimp128_f64_encode`] for `values`. Comparable against
/// the raw `8 * len`, the Gorilla baseline [`xor_f64_bytes`], and depth-1 [`chimp_f64_bytes`].
#[must_use]
pub fn chimp128_f64_bytes(values: &[f64]) -> usize {
	chimp128_f64_encode(values).len()
}

/// The number of fractional decimal digits in `v`'s shortest round-trip representation —
/// `Some(0)` for an integer-valued double, `None` for a non-finite value.
///
/// Rust's `{}` formatter prints an `f64` in its shortest round-tripping *decimal* form (never
/// scientific notation), so the digit count after the point is well defined. This drives the
/// Elf erasing codec's decimal grid.
fn decimal_places(v: f64) -> Option<u32> {
	if !v.is_finite() {
		return None;
	}
	let s = format!("{v}");
	match s.split_once('.') {
		Some((_, frac)) => u32::try_from(frac.len()).ok(),
		None => Some(0),
	}
}

/// Restore an erased value to the column's decimal grid: round `x` to `alpha` decimal places.
///
/// This is the exact function the [`elf_f64_encode`] erasing loop verifies against, so decode
/// reproduces every value bit-for-bit — losslessness holds by construction regardless of the
/// float rounding in the multiply/round/divide.
fn elf_restore(x: f64, p: f64) -> f64 {
	(x * p).round() / p
}

/// Encode an `f64` column with an **Elf-style erasing** codec (roadmap Phase 6.1).
///
/// The f64 slice above [`chimp128_f64_encode`]: losslessly zero each value's low,
/// decimally-insignificant mantissa bits *before* XOR-compressing, so the XOR stream carries
/// long trailing-zero runs.
///
/// Elf's insight (VLDB'23): a double printed to its shortest decimal keeps far more mantissa
/// bits than the decimal needs, and those low bits are noise that wrecks XOR compression. This
/// codec shares one **decimal grid** across the column — `alpha`, the maximum fractional-digit
/// count over all values — and for each value zeroes the largest run of low mantissa bits whose
/// result still rounds back to the original ([`elf_restore`]). The erased stream is then handed
/// to the [`chimp128_f64_encode`] backend; the header is a single `alpha` byte, so the metadata
/// overhead is one byte per column (not per value).
///
/// Returns `None` when the codec is **not applicable** — any non-finite value, an implausibly
/// large `alpha`, or a value that `elf_restore` cannot reproduce even with zero erasing (a float
/// rounding edge). A `None` column simply is not an Elf candidate; when `Some`, the codec is
/// bit-exact for every value. **Advisory only** — this is the "evaluate Elf" slice: it reuses the
/// column-shared-grid, greedy-verified-erase design (faithful bit-level erase per the paper's
/// closed-form is the residue) and is benchmarked against the XOR codecs before any adopt
/// decision; nothing is on disk. *(src: Elf, VLDB'23 —
/// <https://www.vldb.org/pvldb/vol16/p1763-li.pdf>)*
#[must_use]
pub fn elf_f64_encode(values: &[f64]) -> Option<Vec<u8>> {
	if values.is_empty() {
		return Some(Vec::new());
	}
	// Column-shared decimal grid: the widest fractional-digit count over all values.
	let mut alpha = 0_u32;
	for &v in values {
		alpha = alpha.max(decimal_places(v)?);
	}
	// f64 shortest reprs never exceed ~17 fractional digits; a larger alpha would overflow the
	// grid scale, so treat it as not-applicable rather than risk a lossy restore.
	if alpha > 17 {
		return None;
	}
	let p = 10_f64.powi(i32::try_from(alpha).unwrap_or(0));
	let mut stored = Vec::with_capacity(values.len());
	for &v in values {
		let bits = v.to_bits();
		// Zero erasing must already reproduce the value; otherwise the grid cannot represent it
		// losslessly and the codec is not applicable to this column.
		if elf_restore(f64::from_bits(bits), p).to_bits() != bits {
			return None;
		}
		// Greedily keep the most-erased pattern (largest low-bit run) that still restores exactly.
		let mut best = bits;
		for e in 1..=52_u32 {
			let candidate = (bits >> e) << e;
			if elf_restore(f64::from_bits(candidate), p).to_bits() == bits {
				best = candidate;
			}
		}
		stored.push(f64::from_bits(best));
	}
	let mut out = Vec::with_capacity(1 + stored.len());
	out.push(u8::try_from(alpha).unwrap_or(0));
	out.extend_from_slice(&chimp128_f64_encode(&stored));
	Some(out)
}

/// Reconstruct `count` `f64` values from an [`elf_f64_encode`] buffer.
///
/// Bit-exact for every value the encoder accepted (the erasing loop verified each against
/// [`elf_restore`]). A `count` of `0` yields an empty vector; a truncated buffer decodes
/// trailing values as if the missing bits were zero rather than panicking.
#[must_use]
pub fn elf_f64_decode(bytes: &[u8], count: usize) -> Vec<f64> {
	if count == 0 {
		return Vec::new();
	}
	let alpha = bytes.first().copied().unwrap_or(0);
	let p = 10_f64.powi(i32::from(alpha));
	let stored = chimp128_f64_decode(bytes.get(1..).unwrap_or(&[]), count);
	stored.into_iter().map(|s| elf_restore(s, p)).collect()
}

/// The realized byte footprint of [`elf_f64_encode`] for `values`, or `None` when the Elf codec
/// is not applicable to the column. Comparable against the raw `8 * len` and the XOR codecs.
#[must_use]
pub fn elf_f64_bytes(values: &[f64]) -> Option<usize> {
	elf_f64_encode(values).map(|b| b.len())
}

/// The name of the smallest available `f64` value codec for `values` and its byte
/// footprint — the choice a codec selector would make.
///
/// Candidates are the three XOR codecs ([`xor_f64_bytes`] Gorilla, [`chimp_f64_bytes`]
/// depth-1 Chimp, [`chimp128_f64_bytes`] Chimp128) and the uncompressed `raw` baseline
/// (`8 * len`, what an `F64` `.dspseg` block writes today). `raw` is a genuine candidate, not
/// just a comparison point: on adversarial data (every XOR spanning the full width) the XOR
/// codecs *exceed* raw by their flag overhead, and a real selector must never pick a codec
/// larger than storing the bytes plainly. Ties resolve to the simpler codec
/// (`raw` > `gorilla` > `chimp` > `chimp128`), so a more complex codec wins only when
/// strictly smaller and an incompressible column keeps the plain layout.
///
/// This is the "benchmark the codecs and pick the winner" step the roadmap requires before
/// adopting an f64 value codec on disk. **Advisory** — no f64 codec is realized in the
/// `.dspseg` writer yet.
#[must_use]
pub fn best_f64_codec(values: &[f64]) -> (&'static str, usize) {
	let gorilla = xor_f64_bytes(values);
	let chimp = chimp_f64_bytes(values);
	let chimp128 = chimp128_f64_bytes(values);
	// Tie-break toward the simpler codec: only replace the incumbent on a strict improvement.
	let mut name = "raw";
	let mut best = values.len() * 8;
	if gorilla < best {
		name = "gorilla";
		best = gorilla;
	}
	if chimp < best {
		name = "chimp";
		best = chimp;
	}
	if chimp128 < best {
		name = "chimp128";
		best = chimp128;
	}
	(name, best)
}

/// The byte footprint of the smallest available `f64` value codec for `values` — the
/// second element of [`best_f64_codec`]. Never exceeds the raw `8 * len`.
#[must_use]
pub fn best_f64_bytes(values: &[f64]) -> usize {
	best_f64_codec(values).1
}

#[cfg(test)]
mod tests {
	use super::*;

	fn assert_bit_exact(values: &[f64], decoded: &[f64], codec: &str) {
		assert_eq!(decoded.len(), values.len(), "{codec} decodes the right count");
		for (i, (&a, &b)) in values.iter().zip(decoded.iter()).enumerate() {
			// Bit-exact comparison so NaN and signed zero are checked by pattern.
			assert_eq!(a.to_bits(), b.to_bits(), "{codec} value {i} round-trips: {a} vs {b}");
		}
	}

	/// Every fixture exercises *all three* f64 codecs — the Gorilla baseline, the depth-1
	/// Chimp variant, and faithful Chimp128 must each round-trip bit-exactly.
	fn round_trips(values: &[f64]) {
		let g = xor_f64_encode(values);
		assert_bit_exact(values, &xor_f64_decode(&g, values.len()), "gorilla");
		assert_eq!(xor_f64_bytes(values), g.len());
		let c = chimp_f64_encode(values);
		assert_bit_exact(values, &chimp_f64_decode(&c, values.len()), "chimp");
		assert_eq!(chimp_f64_bytes(values), c.len());
		let c128 = chimp128_f64_encode(values);
		assert_bit_exact(values, &chimp128_f64_decode(&c128, values.len()), "chimp128");
		assert_eq!(chimp128_f64_bytes(values), c128.len());
	}

	#[test]
	fn empty_column_encodes_to_nothing() {
		assert!(xor_f64_encode(&[]).is_empty());
		assert_eq!(xor_f64_decode(&[], 0), Vec::<f64>::new());
		assert_eq!(xor_f64_bytes(&[]), 0);
	}

	#[test]
	fn single_value_stores_the_raw_pattern() {
		let values = [3.141_592_653_589_793];
		round_trips(&values);
		// One value is 64 bits = 8 bytes exactly.
		assert_eq!(xor_f64_bytes(&values), 8);
	}

	#[test]
	fn repeated_values_cost_one_bit_each() {
		// 64 identical values: 64 bits for the first + 63 control-zero bits = 127 bits
		// → 16 bytes, versus 512 raw. A constant series is nearly free.
		let values = [42.0_f64; 64];
		round_trips(&values);
		assert_eq!(xor_f64_bytes(&values), 16);
	}

	#[test]
	fn stable_exponent_series_compresses_below_raw() {
		// Gorilla's real regime: a series with a *stable exponent* whose neighbours differ
		// only in low mantissa bits — a sensor reading drifting around a fixed base (here a
		// value near 1000 rising in 0.001 steps). Consecutive IEEE patterns share sign,
		// exponent, and the high mantissa bits, so the XOR has a long leading-zero run and
		// only a few bits are written per value. (Contrast a signal that crosses zero: its
		// exponent sweeps wildly, the XOR spans the full width, and Gorilla cannot help —
		// an honest limitation, so this test does not claim a win it cannot deliver.)
		let values: Vec<f64> = (0..256).map(|i| 1000.0 + f64::from(i) * 0.001).collect();
		round_trips(&values);
		let raw = values.len() * 8;
		let coded = xor_f64_bytes(&values);
		assert!(coded < raw, "gorilla {coded} must beat raw {raw} on a stable-exponent series");
	}

	#[test]
	fn window_reuse_path_round_trips() {
		// A series that repeatedly XORs into the same leading/trailing window exercises
		// the control-bit-0 "reuse previous window" branch.
		let values: Vec<f64> = (0..128).map(|i| 100.0 + f64::from(i % 2)).collect();
		round_trips(&values);
	}

	#[test]
	fn special_values_are_bit_exact() {
		// NaN, both infinities, both signed zeros, subnormals, and the extremes must all
		// round-trip by bit pattern (the codec never interprets the number).
		let values = [
			f64::NAN,
			f64::INFINITY,
			f64::NEG_INFINITY,
			0.0,
			-0.0,
			f64::MIN_POSITIVE,
			f64::from_bits(1), // smallest subnormal
			f64::MAX,
			f64::MIN,
			1.0,
			-1.0,
		];
		round_trips(&values);
	}

	#[test]
	fn random_walk_round_trips_and_compresses() {
		// A deterministic pseudo-random walk with small steps: a realistic irregular
		// sensor-like signal. Verifies both correctness and a real compression win.
		let mut state = 0x2545_F491_4F6C_DD1D_u64;
		let mut x = 500.0_f64;
		let mut values = Vec::with_capacity(512);
		for _ in 0..512 {
			// xorshift64* for a reproducible step in [-0.5, 0.5).
			state ^= state >> 12;
			state ^= state << 25;
			state ^= state >> 27;
			let step = ((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1_u64 << 53) as f64) - 0.5;
			x += step;
			values.push(x);
		}
		round_trips(&values);
		assert!(xor_f64_bytes(&values) < values.len() * 8);
	}

	#[test]
	fn worst_case_full_width_xor_round_trips() {
		// Alternating patterns whose XOR spans the full 64 bits (leading = trailing = 0,
		// significant = 64) exercise the (significant - 1) length encoding at its ceiling.
		let values = [f64::from_bits(0x0000_0000_0000_0000), f64::from_bits(0xFFFF_FFFF_FFFF_FFFF), f64::from_bits(0x0000_0000_0000_0000)];
		round_trips(&values);
	}

	#[test]
	fn chimp_and_gorilla_are_close_on_a_stable_exponent_series() {
		// Honest finding: single-predecessor Chimp is NOT a universal win over Gorilla. On a
		// smooth ramp (a value near 1000 rising in 0.001 steps) Chimp's cheaper leading
		// header is offset by giving up Gorilla's trailing-zero trim in the reuse path, so
		// the two land within ~1% of each other (measured chimp 1350 B vs gorilla 1344 B) —
		// both far below raw. Chimp's real advantage is the new-window trim regime
		// (`chimp_beats_gorilla_on_fresh_windows_with_long_trailing_runs`); the big win is
		// Chimp128's 128-value reference window, filed as the next slice.
		let values: Vec<f64> = (0..256).map(|i| 1000.0 + f64::from(i) * 0.001).collect();
		round_trips(&values);
		let gorilla = xor_f64_bytes(&values);
		let chimp = chimp_f64_bytes(&values);
		let raw = values.len() * 8;
		assert!(chimp < raw && gorilla < raw, "both beat raw {raw}: chimp {chimp}, gorilla {gorilla}");
		let margin = gorilla.max(chimp) - gorilla.min(chimp);
		assert!(margin * 50 < raw, "chimp {chimp} and gorilla {gorilla} stay within ~2% on a smooth ramp");
	}

	#[test]
	fn best_f64_selector_picks_the_true_min_and_never_exceeds_raw() {
		// The selector must equal the smallest of {raw, gorilla, chimp} on every corpus, and
		// its label must name that codec.
		let ramp: Vec<f64> = (0..128).map(|i| 500.0 + f64::from(i) * 0.01).collect();
		let walk_seed_state = 0x1234_5678_9abc_def0_u64;
		let mut s = walk_seed_state;
		let mut x = 42.0;
		let mut walk = Vec::with_capacity(200);
		for _ in 0..200 {
			s ^= s >> 12;
			s ^= s << 25;
			s ^= s >> 27;
			x += ((s >> 40) as f64 / (1_u64 << 24) as f64) - 0.5;
			walk.push(x);
		}
		for series in [&ramp, &walk] {
			let raw = series.len() * 8;
			let (name, bytes) = best_f64_codec(series);
			let expected = raw.min(xor_f64_bytes(series)).min(chimp_f64_bytes(series)).min(chimp128_f64_bytes(series));
			assert_eq!(bytes, expected, "selector must pick the true min");
			assert_eq!(bytes, best_f64_bytes(series));
			assert!(bytes <= raw, "best {bytes} must never exceed raw {raw}");
			let claimed = match name {
				"gorilla" => xor_f64_bytes(series),
				"chimp" => chimp_f64_bytes(series),
				"chimp128" => chimp128_f64_bytes(series),
				_ => raw,
			};
			assert_eq!(claimed, bytes, "label {name} must match the chosen size");
		}
	}

	#[test]
	fn best_f64_selector_falls_back_to_raw_on_incompressible_data() {
		// Adversarial: 64 distinct pseudo-random 64-bit patterns. No value repeats (so no
		// windowed flag-00 shortcut) and every XOR spans most of the width, so all three XOR
		// codecs pay their flag overhead on top of ~64 bits/value and *exceed* raw. The selector
		// must fall back to the plain layout rather than pick a codec bigger than raw.
		let mut s = 0x9E37_79B9_7F4A_7C15_u64;
		let values: Vec<f64> = (0..64)
			.map(|_| {
				s ^= s << 13;
				s ^= s >> 7;
				s ^= s << 17;
				f64::from_bits(s)
			})
			.collect();
		let raw = values.len() * 8;
		let (name, bytes) = best_f64_codec(&values);
		assert_eq!(name, "raw", "incompressible data must fall back to raw");
		assert_eq!(bytes, raw);
		assert!(xor_f64_bytes(&values) > raw && chimp_f64_bytes(&values) > raw && chimp128_f64_bytes(&values) > raw, "all three XOR codecs must exceed raw on incompressible data");
	}

	#[test]
	fn chimp128_beats_depth1_codecs_on_a_revisiting_signal() {
		// Chimp128's decisive regime, unreachable by the single-predecessor codecs: a signal
		// that cycles over a small set of exact levels with period 4. Each sample's immediate
		// predecessor is an unrelated level (Gorilla/depth-1 Chimp must XOR against it, paying a
		// wide meaningful window), but the value exactly four steps back is identical — so the
		// 128-value reference window finds it (flag `00`, ~9 bits). The levels carry full
		// mantissas so their low 14 bits differ, keying each to its own ring slot in the hash
		// (a "nice" value like 1.5 has zero low mantissa bits and would collide — real sensor
		// levels do not).
		let cycle = [12.345_678_9_f64, 78.901_234_5, 34.567_890_1, 90.123_456_7];
		let values: Vec<f64> = (0..256).map(|i| cycle[i % 4]).collect();
		round_trips(&values);
		let raw = values.len() * 8;
		let gorilla = xor_f64_bytes(&values);
		let chimp = chimp_f64_bytes(&values);
		let chimp128 = chimp128_f64_bytes(&values);
		assert!(chimp128 < gorilla, "chimp128 {chimp128} must beat gorilla {gorilla} on a revisiting signal");
		assert!(chimp128 < chimp, "chimp128 {chimp128} must beat depth-1 chimp {chimp} on a revisiting signal");
		assert!(chimp128 < raw, "chimp128 {chimp128} must beat raw {raw}");
		// The selector picks it when it is the strict winner.
		assert_eq!(best_f64_codec(&values).0, "chimp128", "selector must name chimp128 on its winning regime");
	}

	#[test]
	fn chimp128_windowed_flag01_path_round_trips() {
		// A revisiting signal where each level drifts by a tiny high-mantissa step every cycle,
		// so the four-back reference XORs to a *nonzero* value with a long trailing-zero run —
		// exercising the windowed flag-`01` trim branch (not just the exact-repeat flag `00`).
		let bases = [0x4000_0000_0000_0000_u64, 0x4010_0000_0000_0000, 0x4020_0000_0000_0000, 0x4030_0000_0000_0000];
		let values: Vec<f64> = (0..256).map(|i| f64::from_bits(bases[i % 4] + ((i as u64 / 4) << 44))).collect();
		round_trips(&values);
	}

	#[test]
	fn chimp128_ring_wraps_past_128_values() {
		// A series longer than the 128-value window with a long-period revisit (period 96 < 128)
		// verifies the ring buffer and the `count - cand < PREVIOUS_VALUES` window guard across a
		// wrap: references older than 128 must be rejected, fresher ones honoured.
		let values: Vec<f64> = (0..400).map(|i| f64::from(i % 96) * 2.5 + 10.0).collect();
		round_trips(&values);
	}

	/// Elf must round-trip bit-exactly on every column it accepts.
	fn elf_round_trips(values: &[f64]) {
		let encoded = elf_f64_encode(values).expect("elf accepts this column");
		let decoded = elf_f64_decode(&encoded, values.len());
		assert_bit_exact(values, &decoded, "elf");
		assert_eq!(elf_f64_bytes(values), Some(encoded.len()));
	}

	#[test]
	fn elf_round_trips_a_two_decimal_sensor_stream() {
		// A realistic 2-decimal sensor stream (the Elf regime): each reading needs only two
		// decimal places, so ~30 low mantissa bits are decimal noise Elf can erase losslessly.
		// Built as integer/100 so every value is exactly on the 2-decimal grid (accumulating
		// `i * 0.01` would drift off it and print with spurious extra digits).
		let values: Vec<f64> = (0..256).map(|i| f64::from(2000 + i) / 100.0).collect();
		elf_round_trips(&values);
	}

	#[test]
	fn elf_beats_the_xor_codecs_on_low_precision_decimals() {
		// Elf's decisive regime: distinct values carrying only a few significant decimals but
		// full mantissas. Because they never exactly repeat, Chimp128's reference window cannot
		// shortcut them — but erasing the decimally-insignificant low bits gives the backend long
		// trailing-zero XOR runs, so Elf undercuts every raw-mantissa XOR codec.
		let values: Vec<f64> = (0..256).map(|i| f64::from(10000 + i) / 100.0).collect();
		elf_round_trips(&values);
		let raw = values.len() * 8;
		let elf = elf_f64_bytes(&values).expect("elf applies to a 2-decimal column");
		let chimp128 = chimp128_f64_bytes(&values);
		let gorilla = xor_f64_bytes(&values);
		assert!(elf < raw, "elf {elf} must beat raw {raw}");
		assert!(elf < chimp128, "elf {elf} must beat chimp128 {chimp128} by erasing decimal noise");
		assert!(elf < gorilla, "elf {elf} must beat gorilla {gorilla} by erasing decimal noise");
	}

	#[test]
	fn elf_is_not_applicable_to_non_finite_values() {
		// A column with a non-finite value has no decimal grid, so Elf declines rather than
		// risk a lossy restore — the advisory simply is not a candidate there.
		let values = [1.0, f64::NAN, 2.0];
		assert_eq!(elf_f64_encode(&values), None);
		assert_eq!(elf_f64_bytes(&values), None);
	}

	#[test]
	fn elf_round_trips_integers_and_the_empty_column() {
		// Integer-valued doubles (alpha = 0) restore as identity; the empty column is trivially
		// applicable.
		assert_eq!(elf_f64_encode(&[]), Some(Vec::new()));
		let ints: Vec<f64> = (0..64).map(|i| f64::from(i) * 3.0).collect();
		elf_round_trips(&ints);
	}

	#[test]
	fn chimp_never_blows_up_versus_raw_across_regimes() {
		// The honest cross-regime pin: single-predecessor Chimp is not a universal win over
		// Gorilla (Gorilla's reuse-window path cheaply handles *repeated* long-trailing
		// windows that Chimp's `01` trim path must re-header, and Chimp's leading-class
		// rounding writes a few extra significant bits) — but it must never lose to the raw
		// 8 B/value on data with real structure. Chimp128's 128-value reference window is the
		// slice that turns this into a decisive win (filed on the roadmap). Both a smooth ramp
		// and a repeated-window series stay under raw.
		let ramp: Vec<f64> = (0..128).map(|i| 500.0 + f64::from(i) * 0.01).collect();
		let stepped: Vec<f64> = (0..128).map(|i| f64::from_bits(0x4000_0000_0000_0000_u64 + ((i as u64 % 8) << 46))).collect();
		for series in [&ramp, &stepped] {
			round_trips(series);
			assert!(chimp_f64_bytes(series) < series.len() * 8, "chimp must beat raw on structured data");
		}
	}

	#[test]
	fn chimp_repeated_values_cost_two_bits_each() {
		// A constant series: Chimp's flag `00` is two bits per repeat (Gorilla's is one), so
		// 64 identical values cost 64 (first) + 63*2 = 190 bits → 24 bytes. Cheaper than raw
		// 512, and the price of Chimp's four-way flag — a documented trade, not a regression.
		let values = [7.5_f64; 64];
		round_trips(&values);
		assert_eq!(chimp_f64_bytes(&values), 24);
	}

	#[test]
	fn chimp_long_trailing_run_uses_the_trim_path() {
		// Values differing only in high mantissa bits give the XOR a long trailing-zero run
		// (> 6), exercising Chimp's flag-`01` trim branch across many values.
		let values: Vec<f64> = (0..96).map(|i| f64::from_bits(0x4000_0000_0000_0000_u64 + ((i as u64) << 40))).collect();
		round_trips(&values);
	}
}
