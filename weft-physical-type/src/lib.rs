//! # weft-physical-type
//!
//! Physical numeric encodings for WeftDB aspects (roadmap **Phase 4.1**, priority #5
//! in the updated order; Immediate Next Action #8).
//!
//! ## Why this crate exists
//!
//! `BigDecimal` is WeftDB's **logical/API value type** and stays that way (hard
//! constraint #4: *precision-aware, not precision-taxed*). But carrying a
//! heap-allocated arbitrary-precision decimal through the measurement hot path —
//! storage, SIMD, GPU upload, interpolation — is far too slow to win the
//! benchmarks the roadmap is built around. So each aspect *declares* a
//! [`PhysicalType`]: the fast, fixed-width encoding its values execute in.
//!
//! The non-negotiable rule the roadmap states twice is that downcasting is
//! **explicit, schema-declared, and never silent**. This crate enforces that at
//! the type level: [`PhysicalType::encode`] always returns the [`Exactness`] of
//! the conversion alongside the encoded value, so a caller can never lose
//! precision without being handed the residual error to gate on a schema-level
//! bound. A `BigDecimal -> f64` that drops digits is a *reported* `Lossy`, not a
//! quiet truncation.
//!
//! ## Shape
//!
//! - [`PhysicalType`] — the schema-declared encoding. All six named in Phase
//!   4.1 are present: [`F64`], [`F32`], [`ScaledI64`], [`ScaledI128`],
//!   [`Decimal128`], [`BigDecimalText`].
//! - [`PhysicalValue`] — one value held in its encoded physical form.
//! - [`PhysicalType::encode`] — `BigDecimal` -> [`Encoded`] (`PhysicalValue` +
//!   [`Exactness`]).
//! - The **one decimal domain** (ROB-6) — every value's plain form is at most
//!   [`MAX_PLAIN_DIGITS`] digits, enforced by [`parse_decimal`] at parse sites
//!   and by [`PhysicalType::encode`] at the sink.
//! - [`PhysicalValue::to_logical`] — the always-available inverse, reconstructing
//!   the `BigDecimal` the encoding represents.
//! - [`PhysicalType::profile`] — declarative per-encoding metadata
//!   ([`PhysicalProfile`]: storage width, lossless/hot-path eligibility).
//! - [`column`](mod@column) — batch [`encode_column`] of a whole column under one encoding,
//!   aggregating exactness and estimating storage bytes (Storage-v2 / bytes-per-
//!   point prep).
//! - [`timestamp`] — Phase 4.2 integer-epoch timestamp codecs: lossless
//!   delta / delta-of-delta transforms ([`encode_delta`], [`encode_delta_of_delta`])
//!   plus a zig-zag + varint byte estimate, the timestamp half of bytes/point.
//! - [`segment`] — Phase 4.3 in-memory typed columnar [`Segment`]: a value
//!   column, a timestamp column, and per-segment min/max/count stats, with an
//!   exact encode/decode round trip and a `bytes_per_point` matching the bench.
//! - [`page`] — Phase 4.3/4.4 intra-segment paging: [`PagedSegment`] splits a
//!   segment's rows into fixed-height [`Page`]s, each with its own min/max ts/value
//!   stats, so a range query skips pages *within* a segment
//!   ([`PagedSegment::prune_pages_by_time`] / [`PagedSegment::read_time_range`]).
//! - [`weftseg`] — Phase 4.3 on-disk `.weftseg` framing: the hand-rolled, versioned,
//!   CRC-checked byte layout a [`Segment`] seals to (byte primitives + column and
//!   frame codecs).
//! - [`schema`] — Phase 4.1/4.3 [`AspectSchema`]: the per-aspect *declaration* of
//!   the physical value encoding, its error bound, and the timestamp unit;
//!   [`AspectSchema::seal`] builds a [`Segment`] under the declared encoding,
//!   erroring rather than silently downcasting past the declared tolerance.
//! - [`catalog`] — Phase 4.3 control-plane index: a [`SegmentDescriptor`] per
//!   sealed segment (min/max ts/value, row/null counts, byte length, path) and a
//!   [`SegmentIndex`] that prunes a query to the segments it must open
//!   ([`SegmentIndex::prune_by_time`]) without reading a `.weftseg` byte — the
//!   resident shape of the libSQL `segment_index` the next slice persists.
//! - [`tags`] — B-tags series identity: a validated [`TagSet`], its canonical
//!   [`SeriesKey`] bytes (what a frame and the control plane store, so binding a frame to
//!   its series is a byte compare), and the [`SeriesSelector`] a query picks series with.
//!   The canonical form and its caps are frozen format.
//!
//! [`F64`]: PhysicalType::F64
//! [`F32`]: PhysicalType::F32
//! [`ScaledI64`]: PhysicalType::ScaledI64
//! [`ScaledI128`]: PhysicalType::ScaledI128
//! [`Decimal128`]: PhysicalType::Decimal128
//! [`BigDecimalText`]: PhysicalType::BigDecimalText
//!
//! ## Cargo features
//!
//! Every feature is **off by default**, sits **outside the 1.0 semver promise** (its items may
//! change or disappear in any release), and is **pending patent review** (ROADMAP.md, the
//! Phase 6.1 codec patent/licence gate). None of them is needed to read or write a frame under
//! the default configuration, that is with [`FrameOptions::transposed_max_overhead`] unset. A
//! frame written with that option set (as builds from before v0.1.0 did whenever it was set)
//! needs `bitsliced-codec`.
//!
//! - `experimental-codecs` — the advisory float and timestamp codecs the `.weftseg` writer
//!   never emits: the `floatcodec` module (Gorilla-XOR, Chimp, Chimp128, Elf, and the
//!   `best_f64_codec` / `best_f64_bytes` selector), the matching `ColumnEncoding` estimates
//!   (`gorilla_f64_bytes`, `best_f64_bytes`, `best_f64_codec`), and the Sprintz FIRE
//!   forecaster (`fire_residuals`, `fire_reconstruct`, `fire_estimated_bytes`). They exist
//!   so the benchmark harness can report what-if sizes; they are not a storage format.
//! - `bitsliced-codec` — the opt-in bit-sliced (bit-plane-major) `ScaledI64` value codec
//!   (`VAL_CODEC_TRANSPOSED`, codec name `scaled_transposed`): the `transpose_bitpack_*`
//!   primitives and `TRANSPOSE_TILE` in [`timestamp`], the `ColumnEncoding` entries
//!   (`transposed_value_bytes`, `transposed_overhead`, `best_value_codec_transposed`), and
//!   `weftseg::write_value_column_transposed`. Without it the writers ignore
//!   [`FrameOptions::transposed_max_overhead`] and every reader returns
//!   [`WeftSegError::CodecNotEnabled`] for a block that uses the codec, so a store that holds
//!   one fails loudly rather than misreading it.
//!
//! ## Vendor-neutrality
//!
//! Per the workspace hard constraints this crate carries no vendor-specific
//! dependencies: it is a pure numeric encoding layer over `bigdecimal`, usable by
//! the store (`database`), the server (`weft-server`), the harness (`weft-bench`),
//! and the forthcoming Storage v2 columnar segments alike.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

pub mod catalog;
pub mod column;
#[cfg(feature = "experimental-codecs")]
pub mod floatcodec;
/// The `.weftseg` frame version registry, and the two classes of frame-fault a read
/// can fail with (1.0 plan X-FRAME, resolution C5). Also the
/// [`DecodeLimits`] a frame decode enforces (ROB-8).
pub mod frame_registry;
pub mod nulls;
pub mod page;
pub mod schema;
pub mod segment;
pub mod split;
pub mod tags;
pub mod timestamp;
pub mod weftseg;

use std::str::FromStr;

use bigdecimal::{
	num_bigint::{BigInt, Sign}, BigDecimal, FromPrimitive, ToPrimitive
};
pub use catalog::{SegmentDescriptor, SegmentIndex};
pub use column::{encode_column, recommend_encoding, CascadeInner, ColumnEncodeError, ColumnEncoding, DeltaCascadePlan};
#[cfg(feature = "experimental-codecs")]
pub use floatcodec::{best_f64_bytes, best_f64_codec, chimp128_f64_bytes, chimp128_f64_decode, chimp128_f64_encode, chimp_f64_bytes, chimp_f64_decode, chimp_f64_encode, elf_f64_bytes, elf_f64_decode, elf_f64_encode, xor_f64_bytes, xor_f64_decode, xor_f64_encode};
pub use frame_registry::{is_known_extension, layout_of, DecodeLimits, FrameExtension, FrameExtensionSpec, FrameFault, FrameInfo, FrameLayout, FRAME_ENVELOPE, FRAME_PAGED, FRAME_SINGLE_BLOCK, FRAME_VERSION_MAX_KNOWN, KNOWN_EXTENSIONS, MAX_FRAME_EXTENSIONS, MAX_FRAME_PAGES, MAX_FRAME_ROWS};
pub use nulls::{NullMask, NullMaskError};
pub use page::{Page, PagedSegment, DEFAULT_ROWS_PER_PAGE, PAGED_SEGMENT_FORMAT_VERSION};
pub use schema::{AspectSchema, SealError};
pub use segment::{prune_by_time, prune_by_value, prune_present_by_time, Segment, SegmentError, SegmentStats, SEGMENT_FORMAT_VERSION};
use serde::{Deserialize, Serialize};
pub use split::{merge_newer_wins, split_index, SplitDecision, SplitPolicy};
pub use tags::{SeriesKey, SeriesSelector, TagError, TagSet, MAX_SERIES_KEY_BYTES, MAX_TAGS, MAX_TAG_KEY_BYTES, MAX_TAG_VALUE_BYTES, RESERVED_TAG_KEY_PREFIX, TAG_KEY_VALUE_SEPARATOR, TAG_PAIR_SEPARATOR};
pub use timestamp::{bitpack_bytes, bitpack_decode, bitpack_encode, bitpack_width, decode_delta, decode_delta_of_delta, encode_delta, encode_delta_of_delta, first_order_violation, rle_decode, rle_encode, rle_varint_bytes, uvarint_len, zigzag_varint_bytes, zigzag_varint_len, DeltaColumn, DeltaOfDeltaColumn, TimeUnit};
#[cfg(feature = "experimental-codecs")]
pub use timestamp::{fire_estimated_bytes, fire_reconstruct, fire_residuals};
pub use weftseg::{crc32, describe_frame, frame_fault_of, frame_layout_of, frame_value_codec, frame_version, read_frame_paged_segment, read_frame_point, read_frame_points, read_frame_range, read_frame_range_physical, read_frame_segment, read_paged_segment, read_segment, unwrap_frame_envelope, wrap_frame_envelope, write_paged_segment, write_segment, write_segment_with, ByteReader, ByteWriter, FrameOptions, WeftSegError};

/// A schema-declared physical encoding for an aspect's numeric values.
///
/// The variant fixes how every value in the aspect is laid out on the hot path.
/// `BigDecimal` remains the logical type the API speaks; this is the *physical*
/// representation it executes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhysicalType {
	/// IEEE-754 binary64. Eight bytes, SIMD- and GPU-friendly, the default fast
	/// path. Exact only for values that are exactly representable in binary
	/// floating point (e.g. `0.5`, `2.25`); `0.1` and most decimal fractions
	/// encode `Lossy`.
	F64,
	/// IEEE-754 binary32. Four bytes — half the width (and memory bandwidth) of
	/// [`F64`](Self::F64), the cheapest GPU-upload path — at the cost of ~7
	/// significant decimal digits. Exact only for binary-representable values
	/// within that precision; `NotFinite` above the binary32 range (~3.4e38).
	F32,
	/// Fixed-point: the value is stored as a signed 64-bit mantissa interpreted
	/// as `mantissa * 10^(-scale)`. Exact for any value whose magnitude fits an
	/// `i64` once shifted by `scale` decimal places and which has no more than
	/// `scale` fractional digits — the common case for sensor data with a known
	/// number of decimals. Integer-fast, GPU/SIMD-friendly.
	ScaledI64 {
		/// Number of fractional decimal digits the mantissa carries.
		scale: u8,
	},
	/// Fixed-point with a 128-bit mantissa: `mantissa * 10^(-scale)`. The wide
	/// sibling of [`ScaledI64`](Self::ScaledI64) for aspects whose shifted
	/// magnitude exceeds `i64` (~9.2e18) — high-precision financial or
	/// scientific values — while staying integer-exact within `scale`.
	ScaledI128 {
		/// Number of fractional decimal digits the mantissa carries.
		scale: u8,
	},
	/// Decimal floating point with a 128-bit mantissa and a **per-value** scale:
	/// `mantissa * 10^(-scale)`. Unlike the fixed-scale `ScaledI*` encodings the
	/// exponent rides with each value, so it represents any `BigDecimal` whose
	/// significant digits fit `i128` (up to 38 digits) exactly, across a wide
	/// dynamic range — values too large simply lose low-order digits (reported
	/// `Lossy`), never erroring.
	Decimal128,
	/// Lossless decimal text — the verbatim plain-string form of the
	/// `BigDecimal`. Always [`Exactness::Exact`] for every finite value, at the
	/// cost of variable width and parse-on-read. The escape hatch for aspects
	/// that must not lose a single digit.
	BigDecimalText,
}

/// A single value held in its encoded physical form.
///
/// Produced by [`PhysicalType::encode`] and turned back into a `BigDecimal` by
/// [`PhysicalValue::to_logical`]. The variant always matches the [`PhysicalType`]
/// it was encoded under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PhysicalValue {
	/// An [`PhysicalType::F64`]-encoded value. Invariant: always finite (encode
	/// rejects non-finite results).
	F64(f64),
	/// An [`PhysicalType::F32`]-encoded value. Invariant: always finite.
	F32(f32),
	/// A [`PhysicalType::ScaledI64`]-encoded value: `mantissa * 10^(-scale)`.
	ScaledI64 {
		/// The fixed-point mantissa.
		mantissa: i64,
		/// The fractional-digit scale shared by the aspect.
		scale: u8,
	},
	/// A [`PhysicalType::ScaledI128`]-encoded value: `mantissa * 10^(-scale)`.
	ScaledI128 {
		/// The 128-bit fixed-point mantissa.
		mantissa: i128,
		/// The fractional-digit scale shared by the aspect.
		scale: u8,
	},
	/// A [`PhysicalType::Decimal128`]-encoded value: `mantissa * 10^(-scale)`
	/// with a per-value `scale`.
	Decimal128 {
		/// The 128-bit significand.
		mantissa: i128,
		/// This value's own base-10 scale (negative for magnitudes above 1).
		scale: i64,
	},
	/// A [`PhysicalType::BigDecimalText`]-encoded value: the plain-string decimal.
	BigDecimalText(String),
}

/// Whether an [`encode`](PhysicalType::encode) preserved the logical value
/// exactly, and if not, by how much it missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exactness {
	/// The encoded value reconstructs to the original `BigDecimal` bit-for-bit.
	Exact,
	/// The encoding lost precision; `abs_error` is `|original - decoded|`
	/// (normalized), for a caller to compare against a schema-level error bound.
	Lossy {
		/// The absolute reconstruction error, `|original - decoded|`.
		abs_error: BigDecimal,
	},
}

impl Exactness {
	/// `true` iff the encoding was exact.
	#[must_use]
	pub const fn is_exact(&self) -> bool {
		matches!(self, Self::Exact)
	}

	/// The absolute reconstruction error: zero when [`Exact`](Self::Exact),
	/// otherwise the stored `abs_error`.
	#[must_use]
	pub fn abs_error(&self) -> BigDecimal {
		match self {
			Self::Exact => BigDecimal::from(0),
			Self::Lossy { abs_error } => abs_error.clone(),
		}
	}
}

/// The result of an [`encode`](PhysicalType::encode): the physical value plus the
/// [`Exactness`] of the conversion, so loss can never be silent.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
	/// The value in its physical encoding.
	pub value: PhysicalValue,
	/// Whether the encoding was lossless, and the residual error if not.
	pub exactness: Exactness,
}

/// Why a value could not be encoded under a given [`PhysicalType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
	/// The value's magnitude does not fit the target integer encoding (the
	/// shifted mantissa overflows `i64` for [`PhysicalType::ScaledI64`]).
	Overflow,
	/// The value could not be represented as a finite `f64` (its magnitude
	/// exceeds the binary64 range) for [`PhysicalType::F64`].
	NotFinite,
	/// The value is outside the one decimal domain (ROB-6): its plain
	/// (non-exponent) form carries more than [`MAX_PLAIN_DIGITS`] digits.
	DecimalOutOfRange {
		/// How many digits the value's plain form carries.
		digits: usize,
	},
}

impl std::fmt::Display for EncodeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Overflow => f.write_str("value magnitude overflows the target integer encoding"),
			Self::NotFinite => f.write_str("value is not representable as a finite f64"),
			Self::DecimalOutOfRange { digits } => write!(f, "value's plain form has {digits} digits, over the {MAX_PLAIN_DIGITS}-digit decimal domain bound"),
		}
	}
}

impl std::error::Error for EncodeError {}

/// The one decimal domain every WeftDB value lives in (1.0 plan **decision 10**,
/// slice **ROB-6**, resolving conflict **C13**): a value's **plain**
/// (non-exponent) form — the form every sink writes, see
/// [`PhysicalType::BigDecimalText`]'s `to_plain_string` — carries at most
/// [`MAX_PLAIN_DIGITS`] digits.
///
/// This is the *only* decimal bound. It supersedes the freeze track's FRE-8a
/// rule (≤1,000 significant digits and |exp| ≤ 6,144), which is **not** kept
/// alongside: that rule still allowed ~7 KB per value precisely because sinks
/// write the plain form, so plain length is the quantity to bound. The bound
/// covers the full `f64` range: the largest finite `f64` needs 309 plain
/// digits, the smallest subnormal's shortest round-trip form (`5e-324`) needs
/// 325, and the exact decimal expansion of `f64::MIN_POSITIVE` needs 1,023.
pub const MAX_PLAIN_DIGITS: usize = 1_024;

/// How many digits the plain (non-exponent) form of `value` carries: the digit
/// characters of `value.to_plain_string()` (sign and decimal point excluded).
///
/// Computed from the `(mantissa, scale)` pair without materialising the plain
/// string, so a hostile exponent (`1e100000000`) is measured arithmetically
/// rather than by allocating the expansion it names. A zero mantissa follows
/// `to_plain_string`'s own spelling (`0.000` is 4 digits, `0e5` writes
/// `000000`, 6 digits).
#[must_use]
pub fn plain_digit_count(value: &BigDecimal) -> usize {
	let (bigint, scale) = value.as_bigint_and_exponent();
	let mantissa = bigint.to_string();
	let mantissa_digits = u128::try_from(mantissa.len() - usize::from(mantissa.starts_with('-'))).unwrap_or(u128::MAX);
	let count = if scale >= 0 {
		let scale = u128::from(u64::try_from(scale).unwrap_or(u64::MAX));
		if mantissa_digits > scale {
			mantissa_digits
		} else {
			scale + 1
		}
	} else {
		mantissa_digits + u128::from(scale.unsigned_abs())
	};
	usize::try_from(count).unwrap_or(usize::MAX)
}

/// A value is outside the one decimal domain: its plain form carries more
/// than [`MAX_PLAIN_DIGITS`] digits (ROB-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecimalOutOfRange {
	/// How many digits the value's plain form carries.
	pub digits: usize,
	/// The bound it exceeded ([`MAX_PLAIN_DIGITS`]).
	pub max: usize,
}

impl std::fmt::Display for DecimalOutOfRange {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "decimal's plain form has {} digits, over the {}-digit bound", self.digits, self.max)
	}
}

impl std::error::Error for DecimalOutOfRange {}

/// Check `value` against the one decimal domain (ROB-6).
///
/// # Errors
///
/// Returns [`DecimalOutOfRange`] when the value's plain form carries more
/// than [`MAX_PLAIN_DIGITS`] digits.
pub fn check_decimal_domain(value: &BigDecimal) -> Result<(), DecimalOutOfRange> {
	let digits = plain_digit_count(value);
	if digits > MAX_PLAIN_DIGITS {
		return Err(DecimalOutOfRange { digits, max: MAX_PLAIN_DIGITS });
	}
	Ok(())
}

/// Why a decimal text could not enter the domain (ROB-6): it either does not
/// parse as a decimal at all, or it parses to a value outside the domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseDecimalError {
	/// The text is not a decimal literal.
	Invalid,
	/// The text parses, but its plain form exceeds the domain bound.
	OutOfRange(DecimalOutOfRange),
}

impl std::fmt::Display for ParseDecimalError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Invalid => f.write_str("text is not a decimal"),
			Self::OutOfRange(e) => write!(f, "{e}"),
		}
	}
}

impl std::error::Error for ParseDecimalError {}

/// Parse a decimal literal straight into the one decimal domain (ROB-6): the
/// parse every parse site uses, so no site can accept a value the sinks would
/// refuse to write.
///
/// # Errors
///
/// [`ParseDecimalError::Invalid`] when `text` does not parse as a decimal, or
/// [`ParseDecimalError::OutOfRange`] when its plain form carries more than
/// [`MAX_PLAIN_DIGITS`] digits.
pub fn parse_decimal(text: &str) -> Result<BigDecimal, ParseDecimalError> {
	let value = BigDecimal::from_str(text).map_err(|_| ParseDecimalError::Invalid)?;
	check_decimal_domain(&value).map_err(ParseDecimalError::OutOfRange)?;
	Ok(value)
}

/// `10^n` as a `BigDecimal`, built without a `Pow` trait import.
fn pow10(n: u8) -> BigDecimal {
	let ten = BigInt::from(10);
	let mantissa = (0..n).fold(BigInt::from(1), |acc, _| acc * &ten);
	BigDecimal::from(mantissa)
}

/// Divide a `BigInt` by 10, rounding half away from zero — one step of the
/// digit-shedding loop that fits a mantissa into `i128` for
/// [`PhysicalType::Decimal128`].
fn round_div_10(n: &BigInt) -> BigInt {
	let bias = if n.sign() == Sign::Minus { BigInt::from(-5) } else { BigInt::from(5) };
	(n + bias) / BigInt::from(10)
}

/// Reduce `(mantissa, scale)` (value = `mantissa * 10^(-scale)`) until the
/// mantissa fits `i128`, shedding one low-order digit per step. Returns the
/// fitted `(i128, scale)`; exactness is decided by the caller via [`measure`].
fn fit_i128(mut mantissa: BigInt, mut scale: i64) -> (i128, i64) {
	loop {
		if let Some(m) = mantissa.to_i128() {
			return (m, scale);
		}
		mantissa = round_div_10(&mantissa);
		scale -= 1;
	}
}

/// Classify the loss between an original value and its reconstruction.
///
/// Zero-ness is decided on the difference's integer mantissa sign, which is
/// robust to `BigDecimal` scale differences (`0` is the only value with a
/// no-sign mantissa).
fn measure(original: &BigDecimal, decoded: &BigDecimal) -> Exactness {
	let diff = original - decoded;
	let (mantissa, _exp) = diff.as_bigint_and_exponent();
	if mantissa.sign() == Sign::NoSign {
		Exactness::Exact
	} else {
		Exactness::Lossy { abs_error: diff.abs().normalized() }
	}
}

/// Declarative metadata for a [`PhysicalType`].
///
/// This realizes the "each declaring storage encoding, eligibility, exactness
/// guarantees" sentence of roadmap Phase 4.1 as data a schema, planner, or
/// benchmark report can consume without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalProfile {
	/// The encoding's stable identifier (see [`PhysicalType::name`]).
	pub name: &'static str,
	/// Fixed per-value storage width in bytes for WeftDB's in-memory/columnar
	/// representation, or `None` for the variable-width
	/// [`BigDecimalText`](PhysicalType::BigDecimalText). (For
	/// [`Decimal128`](PhysicalType::Decimal128) this counts the 16-byte mantissa
	/// plus its 8-byte per-value scale, not IEEE-754 decimal128's packed 16.)
	pub fixed_width_bytes: Option<usize>,
	/// `true` only when the encoding reconstructs **every** finite value exactly
	/// (i.e. [`BigDecimalText`](PhysicalType::BigDecimalText)). The fixed-width
	/// encodings are exact only within their range/scale and otherwise report
	/// [`Exactness::Lossy`].
	pub always_lossless: bool,
	/// `true` when values can live on the SIMD/GPU/columnar hot path as a
	/// fixed-width column — every encoding except the variable-width
	/// [`BigDecimalText`](PhysicalType::BigDecimalText), which must go through
	/// `BigDecimal`.
	pub hot_path_eligible: bool,
}

impl PhysicalType {
	/// The declarative [`PhysicalProfile`] for this encoding.
	#[must_use]
	pub const fn profile(self) -> PhysicalProfile {
		let fixed_width_bytes = match self {
			Self::F32 => Some(4),
			Self::F64 | Self::ScaledI64 { .. } => Some(8),
			Self::ScaledI128 { .. } => Some(16),
			Self::Decimal128 => Some(24),
			Self::BigDecimalText => None,
		};
		PhysicalProfile { name: self.name(), fixed_width_bytes, always_lossless: matches!(self, Self::BigDecimalText), hot_path_eligible: fixed_width_bytes.is_some() }
	}

	/// Encode a logical `BigDecimal` into this physical type, reporting the
	/// [`Exactness`] of the conversion.
	///
	/// # Errors
	///
	/// Returns [`EncodeError::DecimalOutOfRange`] if the value is outside the
	/// one decimal domain (ROB-6: plain form over [`MAX_PLAIN_DIGITS`]
	/// digits) — checked first, for every encoding, because every sink writes
	/// the plain form. Returns [`EncodeError::Overflow`] if the value does not
	/// fit a [`ScaledI64`](Self::ScaledI64) /
	/// [`ScaledI128`](Self::ScaledI128) mantissa, or
	/// [`EncodeError::NotFinite`] if it has no finite [`F64`](Self::F64) /
	/// [`F32`](Self::F32) representation. Lossy-but-representable conversions
	/// are **not** errors — they succeed with [`Exactness::Lossy`] so the
	/// caller decides whether the residual is acceptable.
	pub fn encode(self, value: &BigDecimal) -> Result<Encoded, EncodeError> {
		check_decimal_domain(value).map_err(|e| EncodeError::DecimalOutOfRange { digits: e.digits })?;
		match self {
			Self::F64 => {
				let f = value.to_f64().filter(|f| f.is_finite()).ok_or(EncodeError::NotFinite)?;
				let pv = PhysicalValue::F64(f);
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::F32 => {
				let f = value.to_f32().filter(|f| f.is_finite()).ok_or(EncodeError::NotFinite)?;
				let pv = PhysicalValue::F32(f);
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::ScaledI64 { scale } => {
				let shifted = (value * pow10(scale)).round(0);
				let mantissa = shifted.to_i64().ok_or(EncodeError::Overflow)?;
				let pv = PhysicalValue::ScaledI64 { mantissa, scale };
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::ScaledI128 { scale } => {
				let shifted = (value * pow10(scale)).round(0);
				let mantissa = shifted.to_i128().ok_or(EncodeError::Overflow)?;
				let pv = PhysicalValue::ScaledI128 { mantissa, scale };
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::Decimal128 => {
				let (bigint, scale) = value.as_bigint_and_exponent();
				let (mantissa, scale) = fit_i128(bigint, scale);
				let pv = PhysicalValue::Decimal128 { mantissa, scale };
				let exactness = measure(value, &pv.to_logical());
				Ok(Encoded { value: pv, exactness })
			}
			Self::BigDecimalText => {
				let pv = PhysicalValue::BigDecimalText(value.to_plain_string());
				Ok(Encoded { value: pv, exactness: Exactness::Exact })
			}
		}
	}

	/// The encoding's stable wire/identifier name (e.g. for schema serialization
	/// and debug output).
	#[must_use]
	pub const fn name(self) -> &'static str {
		match self {
			Self::F64 => "f64",
			Self::F32 => "f32",
			Self::ScaledI64 { .. } => "scaled_i64",
			Self::ScaledI128 { .. } => "scaled_i128",
			Self::Decimal128 => "decimal128",
			Self::BigDecimalText => "bigdecimal_text",
		}
	}
}

impl PhysicalValue {
	/// Reconstruct the logical `BigDecimal` this physical value represents.
	///
	/// Always succeeds: every encoded value produced by
	/// [`PhysicalType::encode`] has a well-defined logical reconstruction. For
	/// the (encode-rejected) degenerate cases — a non-finite `f64` or an
	/// unparseable text — it yields zero rather than panicking.
	#[must_use]
	pub fn to_logical(&self) -> BigDecimal {
		match self {
			Self::F64(f) => BigDecimal::from_f64(*f).unwrap_or_default(),
			Self::F32(f) => BigDecimal::from_f32(*f).unwrap_or_default(),
			Self::ScaledI64 { mantissa, scale } => BigDecimal::new(BigInt::from(*mantissa), i64::from(*scale)),
			Self::ScaledI128 { mantissa, scale } => BigDecimal::new(BigInt::from(*mantissa), i64::from(*scale)),
			Self::Decimal128 { mantissa, scale } => BigDecimal::new(BigInt::from(*mantissa), *scale),
			Self::BigDecimalText(s) => s.parse().unwrap_or_default(),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("test literal parses")
	}

	#[test]
	fn plain_digit_count_matches_to_plain_string() {
		// The arithmetic count must agree with the literal plain form on every
		// shape: integers, fractions, exponent forms, signed values and zeros.
		for lit in ["0", "0.000", "0e5", "1.20", "0.5", "12.34", "-987.650", "1e1023", "1e-1023", "5e-324", "1234567890e1000", "9.99e1023", "0.001", "1000"] {
			let v = bd(lit);
			let literal = v.to_plain_string().chars().filter(char::is_ascii_digit).count();
			assert_eq!(plain_digit_count(&v), literal, "count for {lit}");
		}
	}

	#[test]
	fn decimal_domain_boundary_is_1024_plain_digits() {
		// Exactly at the bound: accepted. One digit over, on either side of
		// the decimal point: refused. (FRE-8a's significant-digit/exponent
		// rule would judge these differently — it is superseded, not kept.)
		assert!(check_decimal_domain(&bd("1e1023")).is_ok(), "1 followed by 1023 zeros is 1024 digits");
		assert!(check_decimal_domain(&bd("1e-1023")).is_ok(), "0. then 1022 zeros then 1 is 1024 digits");
		assert_eq!(check_decimal_domain(&bd("1e1024")), Err(DecimalOutOfRange { digits: 1025, max: MAX_PLAIN_DIGITS }));
		assert_eq!(check_decimal_domain(&bd("1e-1024")), Err(DecimalOutOfRange { digits: 1025, max: MAX_PLAIN_DIGITS }));
		// A long *significant* digit string is refused by plain length too.
		let long = format!("{}.{}", "1".repeat(700), "2".repeat(400));
		assert_eq!(check_decimal_domain(&bd(&long)), Err(DecimalOutOfRange { digits: 1100, max: MAX_PLAIN_DIGITS }));
	}

	#[test]
	fn decimal_domain_covers_the_full_f64_range() {
		// The extremes of the f64 range, as the shortest round-trip text a
		// float input enters as (decision 11) and as exact `from_f64` values.
		for lit in ["1.7976931348623157e308", "-1.7976931348623157e308", "2.2250738585072014e-308", "5e-324", "-5e-324"] {
			assert!(check_decimal_domain(&bd(lit)).is_ok(), "{lit} is inside the domain");
		}
		for f in [f64::MAX, f64::MIN, f64::MIN_POSITIVE, -f64::MIN_POSITIVE] {
			let v = BigDecimal::from_f64(f).expect("finite");
			assert!(check_decimal_domain(&v).is_ok(), "from_f64({f}) is inside the domain");
		}
	}

	#[test]
	fn parse_decimal_enforces_the_domain_at_the_parse_site() {
		assert_eq!(parse_decimal("42.5"), Ok(bd("42.5")));
		assert_eq!(parse_decimal("not-a-decimal"), Err(ParseDecimalError::Invalid));
		assert_eq!(parse_decimal("1e1024"), Err(ParseDecimalError::OutOfRange(DecimalOutOfRange { digits: 1025, max: MAX_PLAIN_DIGITS })));
		// The exponent is measured, not materialised: this must return, not
		// try to allocate a billion-digit string.
		assert!(matches!(parse_decimal("1e1000000000"), Err(ParseDecimalError::OutOfRange(_))));
	}

	#[test]
	fn encode_refuses_out_of_domain_values_for_every_encoding() {
		let over = bd("1e1024");
		for pt in [PhysicalType::F64, PhysicalType::F32, PhysicalType::ScaledI64 { scale: 2 }, PhysicalType::ScaledI128 { scale: 2 }, PhysicalType::Decimal128, PhysicalType::BigDecimalText] {
			assert_eq!(pt.encode(&over), Err(EncodeError::DecimalOutOfRange { digits: 1025 }), "{pt:?} must refuse an out-of-domain value");
		}
		// At the bound every encoding still encodes.
		let at = bd("1e1023");
		assert!(PhysicalType::BigDecimalText.encode(&at).expect("encodes").exactness.is_exact());
	}

	#[test]
	fn f64_exact_for_binary_representable() {
		// 0.5, 2.25 and 128 are all exact in binary64.
		for lit in ["0.5", "2.25", "128", "-0.25"] {
			let v = bd(lit);
			let enc = PhysicalType::F64.encode(&v).expect("encodes");
			assert!(enc.exactness.is_exact(), "{lit} should encode exactly");
			assert_eq!(enc.value.to_logical(), v, "{lit} round-trips");
		}
	}

	#[test]
	fn f64_lossy_for_decimal_fraction() {
		let v = bd("0.1");
		let enc = PhysicalType::F64.encode(&v).expect("encodes");
		match enc.exactness {
			Exactness::Lossy { abs_error } => {
				assert!(abs_error > bd("0"), "error is positive");
				assert!(abs_error < bd("0.0000001"), "but tiny: {abs_error}");
			}
			Exactness::Exact => panic!("0.1 is not exact in binary64"),
		}
	}

	#[test]
	fn f64_not_finite_for_huge_magnitude() {
		// 10^400 has no finite binary64 representation.
		let mut s = String::from("1");
		s.push_str(&"0".repeat(400));
		let v = bd(&s);
		assert_eq!(PhysicalType::F64.encode(&v), Err(EncodeError::NotFinite));
	}

	#[test]
	fn scaled_i64_exact_within_scale() {
		let v = bd("12.34");
		let enc = PhysicalType::ScaledI64 { scale: 2 }.encode(&v).expect("encodes");
		assert_eq!(enc.value, PhysicalValue::ScaledI64 { mantissa: 1234, scale: 2 });
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn scaled_i64_lossy_beyond_scale() {
		// 12.345 at scale 2 rounds to 12.34 or 12.35; either way ~0.005 error.
		let v = bd("12.345");
		let enc = PhysicalType::ScaledI64 { scale: 2 }.encode(&v).expect("encodes");
		match enc.exactness {
			Exactness::Lossy { abs_error } => {
				assert!(abs_error <= bd("0.005"), "rounding error bounded: {abs_error}");
				assert!(abs_error > bd("0"));
			}
			Exactness::Exact => panic!("three fractional digits cannot be exact at scale 2"),
		}
	}

	#[test]
	fn scaled_i64_negative_round_trip() {
		let v = bd("-7.5");
		let enc = PhysicalType::ScaledI64 { scale: 1 }.encode(&v).expect("encodes");
		assert_eq!(enc.value, PhysicalValue::ScaledI64 { mantissa: -75, scale: 1 });
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn scaled_i64_overflow_reported() {
		// 10^30 shifted by scale 2 overflows i64 (~9.2 * 10^18).
		let mut s = String::from("1");
		s.push_str(&"0".repeat(30));
		let v = bd(&s);
		assert_eq!(PhysicalType::ScaledI64 { scale: 2 }.encode(&v), Err(EncodeError::Overflow));
	}

	#[test]
	fn bigdecimal_text_is_always_exact() {
		// A value with far more digits than f64 or i64 could ever hold.
		let v = bd("123456789012345678901234567890.123456789012345678901234567890");
		let enc = PhysicalType::BigDecimalText.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn f32_exact_and_lossy() {
		// 0.5 is exact in binary32; 0.1 is not.
		let exact = PhysicalType::F32.encode(&bd("0.5")).expect("encodes");
		assert!(exact.exactness.is_exact());
		assert_eq!(exact.value, PhysicalValue::F32(0.5));

		let lossy = PhysicalType::F32.encode(&bd("0.1")).expect("encodes");
		assert!(!lossy.exactness.is_exact(), "0.1 is not exact in binary32");
		// f32 loses more than f64, so the error is larger than the f64 case
		// but still small in absolute terms.
		assert!(lossy.exactness.abs_error() < bd("0.0001"));
	}

	#[test]
	fn f32_not_finite_above_range() {
		// ~1e40 exceeds the binary32 range (~3.4e38) but is fine for f64.
		let mut s = String::from("1");
		s.push_str(&"0".repeat(40));
		let v = bd(&s);
		assert_eq!(PhysicalType::F32.encode(&v), Err(EncodeError::NotFinite));
		assert!(PhysicalType::F64.encode(&v).is_ok(), "f64 still holds 1e40");
	}

	#[test]
	fn scaled_i128_holds_values_beyond_i64() {
		// 10^25 shifted by scale 2 = 10^27, far past i64 but inside i128.
		let mut s = String::from("1");
		s.push_str(&"0".repeat(25));
		let v = bd(&s);
		assert_eq!(PhysicalType::ScaledI64 { scale: 2 }.encode(&v), Err(EncodeError::Overflow));
		let enc = PhysicalType::ScaledI128 { scale: 2 }.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn scaled_i128_overflow_reported() {
		// 10^40 shifted by scale 2 overflows even i128 (~1.7 * 10^38).
		let mut s = String::from("1");
		s.push_str(&"0".repeat(40));
		let v = bd(&s);
		assert_eq!(PhysicalType::ScaledI128 { scale: 2 }.encode(&v), Err(EncodeError::Overflow));
	}

	#[test]
	fn decimal128_exact_within_38_digits() {
		// A 30-digit decimal with a fractional part — fits i128 (38 digits) exactly.
		let v = bd("123456789012345678901234.567890");
		let enc = PhysicalType::Decimal128.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact(), "30 significant digits fit i128");
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn decimal128_lossy_but_never_errors_beyond_38_digits() {
		// 50 significant digits cannot fit i128: low-order digits are shed,
		// reported Lossy, and the call still succeeds.
		let v = bd("12345678901234567890123456789012345678901234567890");
		let enc = PhysicalType::Decimal128.encode(&v).expect("never errors");
		match enc.exactness {
			Exactness::Lossy { abs_error } => assert!(abs_error > bd("0")),
			Exactness::Exact => panic!("50 digits cannot be exact in i128"),
		}
		// The reconstruction stays the same order of magnitude as the original.
		let decoded = enc.value.to_logical();
		assert!(decoded > bd("1e49") && decoded < bd("1.3e49"), "magnitude preserved: {decoded}");
	}

	#[test]
	fn decimal128_large_magnitude_round_trips() {
		// A whole number above i64 round-trips exactly via the per-value scale.
		let v = bd("9223372036854775808000"); // (i64::MAX + 1) * 1000
		let enc = PhysicalType::Decimal128.encode(&v).expect("encodes");
		assert!(enc.exactness.is_exact());
		assert_eq!(enc.value.to_logical(), v);
	}

	#[test]
	fn profiles_describe_each_encoding() {
		let f64 = PhysicalType::F64.profile();
		assert_eq!(f64.fixed_width_bytes, Some(8));
		assert!(f64.hot_path_eligible);
		assert!(!f64.always_lossless);

		assert_eq!(PhysicalType::F32.profile().fixed_width_bytes, Some(4));
		assert_eq!(PhysicalType::ScaledI64 { scale: 2 }.profile().fixed_width_bytes, Some(8));
		assert_eq!(PhysicalType::ScaledI128 { scale: 2 }.profile().fixed_width_bytes, Some(16));
		assert_eq!(PhysicalType::Decimal128.profile().fixed_width_bytes, Some(24));

		let text = PhysicalType::BigDecimalText.profile();
		assert_eq!(text.fixed_width_bytes, None);
		assert!(!text.hot_path_eligible, "text cannot live on the fixed-width hot path");
		assert!(text.always_lossless, "text is the only universally lossless encoding");
		assert_eq!(text.name, "bigdecimal_text");
	}

	#[test]
	fn names_are_stable() {
		assert_eq!(PhysicalType::F64.name(), "f64");
		assert_eq!(PhysicalType::F32.name(), "f32");
		assert_eq!(PhysicalType::ScaledI64 { scale: 3 }.name(), "scaled_i64");
		assert_eq!(PhysicalType::ScaledI128 { scale: 3 }.name(), "scaled_i128");
		assert_eq!(PhysicalType::Decimal128.name(), "decimal128");
		assert_eq!(PhysicalType::BigDecimalText.name(), "bigdecimal_text");
	}
}
