//! The bytes every `.weftseg` frame layout writes, pinned as committed goldens (roadmap X-FRAME
//! / slice **FRE-3**).
//!
//! Every other test in this crate pins a *contract* — write bytes, read them back, compare with
//! what went in. That is necessary and it is not enough: a round trip passes whenever a writer and
//! a reader change together, because each is judged against the other rather than against the past,
//! and the failure that matters for a file already on a disk is precisely the two moving apart. The
//! files under `tests/golden/` are what the writer produced on the day they were reviewed, and the
//! assertion is byte-for-byte.
//!
//! Two more things each case pins beyond the bytes, because a golden that only says "these bytes"
//! teaches nothing about *why* those bytes:
//!
//! * **which codec the selector chose** — the label, read back out of the fixture. A selector that
//!   quietly stops picking the blocked codec does not corrupt anything; it silently rewrites every
//!   future file in a different shape, which is a compatibility event and must fail like one.
//! * **what the frame means** — row count, physical type, and a checksum over the decoded
//!   timestamps and values, so a regenerated fixture whose contents drifted cannot be waved through.
//!
//! Regenerating is [`WEFT_UPDATE_GOLDENS`], and it is a format change: see
//! [`golden::harness`](harness).

#[path = "golden/harness.rs"]
mod harness;

use std::str::FromStr;

use bigdecimal::BigDecimal;
use harness::{check_golden, golden_dir, updating};
use weft_physical_type::{describe_frame, frame_layout_of, frame_registry::FrameExtension, frame_value_codec, page::PagedSegment, read_frame_paged_segment, read_frame_segment, unwrap_frame_envelope, wrap_frame_envelope, write_segment_with, FrameLayout, FrameOptions, Segment, TimeUnit, FRAME_ENVELOPE, FRAME_PAGED, FRAME_SINGLE_BLOCK};

/// The tolerance the frames are written at: exact, no lossy float may be selected.
fn exact() -> BigDecimal {
	BigDecimal::from(0)
}

fn dec(text: &str) -> BigDecimal {
	BigDecimal::from_str(text).expect("a valid decimal literal")
}

// ---------------------------------------------------------------------------------------------
// Timestamp corpora. Each one exists to realize *one* delta-of-delta codec on disk; the shape is
// the regime that codec wins, so the corpus is part of the specification, not incidental to it.
// ---------------------------------------------------------------------------------------------

/// Second differences `[1, 2, 3, 1_000_000_000]`: per-value varint adapts to the outlier while a
/// fixed width must widen every value and RLE pays two varints per length-one run, so the plain
/// varint block wins. `delta_of_delta` is also the format's floor — every other codec is an
/// optimisation over this byte layout, so it is the one that must be pinned even when it is dull.
fn ts_plain() -> Vec<i64> {
	vec![0, 1, 3, 7, 14, 21 + 1_000_000_000]
}

/// Perfectly regular 10 ms spacing: all-zero second differences, so fixed-width bit-packing
/// collapses to a header byte and no data.
fn ts_regular(rows: usize) -> Vec<i64> {
	(0..i64::try_from(rows).expect("row count fits i64")).map(|i| 1_700_000_000 + i * 10).collect()
}

/// An accelerating ramp: the second differences are a constant run of 5, which is what run-length
/// encoding is for.
fn ts_rle(rows: usize) -> Vec<i64> {
	let mut stamps = vec![0_i64, 1];
	let mut delta = 1_i64;
	for _ in 0..rows {
		delta += 5;
		stamps.push(stamps.last().copied().expect("seeded") + delta);
	}
	stamps
}

/// Regular spacing with one 1.5 s spike every sixteenth sample: scattered, small, second
/// differences — Gorilla's bucket-prefix regime, where per-value varint still pays a byte each.
fn ts_gorilla(rows: usize) -> Vec<i64> {
	let mut stamps = Vec::with_capacity(rows);
	let mut t = 1_700_000_000_i64;
	for i in 0..rows {
		t += if i % 16 == 15 { 2_500 } else { 1_000 };
		stamps.push(t);
	}
	stamps
}

/// Narrow ±1 jitter with one contiguous wide window: global bit-packing over-pays for every narrow
/// value, a FOR reference is wasted on sign-alternating data, and per-block adaptive packing wins.
fn ts_blocked(rows: usize) -> Vec<i64> {
	let dods: Vec<i64> = (0..i64::try_from(rows).expect("row count fits i64")).map(|i| if (96..128).contains(&i) { 500_000_000 + i } else { (i % 3) - 1 }).collect();
	let mut stamps = vec![1_000, 11_000];
	let mut delta = 10_000_i64;
	for dod in dods {
		delta += dod;
		stamps.push(stamps.last().copied().expect("seeded") + delta);
	}
	stamps
}

/// A constant step with one tick of alternation (`+1000`, `+1001`, ...): the second differences
/// are two bits wide globally, never repeat so there is no run to length-encode, and each 16-delta
/// block is as wide as a 16-step drift, which is what makes global bit-packing cheaper than the
/// per-block codec here rather than the other way round.
fn ts_bitpack(rows: usize) -> Vec<i64> {
	(0..i64::try_from(rows).expect("row count fits i64")).map(|i| 1_700_000_000_000 + i * 1_000 + i / 2).collect()
}

// ---------------------------------------------------------------------------------------------
// Value corpora. One per physical type the selector can land on, plus the three scaled-integer
// codecs whose selection a size comparison decides. Values are written as decimal *text* so the
// logical value is exact and the physical type is a consequence of the data rather than of a
// hand-picked `PhysicalType`.
// ---------------------------------------------------------------------------------------------

/// Small integers: exactly representable in binary32, so the selector's cheapest candidate wins.
fn v_f32(rows: usize) -> Vec<BigDecimal> {
	(0..rows).map(|i| dec(&format!("{}", i % 9))).collect()
}

/// `2^53 + k·2^29`: exact in binary64, two bits above binary32's mantissa, so `F64` is the
/// cheapest *exact* candidate.
fn v_f64(rows: usize) -> Vec<BigDecimal> {
	(0..i64::try_from(rows).expect("row count fits i64")).map(|i| dec(&((1i64 << 53) + i * (1i64 << 29)).to_string())).collect()
}

/// `-0.32 … 0.31` in hundredths: mantissas `-32 … 31`, symmetric about zero (so a FOR reference
/// buys nothing) and narrow enough that fixed width beats the one-byte varint floor.
fn v_scaled_bitpack(rows: usize) -> Vec<BigDecimal> {
	(0..i64::try_from(rows).expect("row count fits i64")).map(|i| dec(&format!("{:.2}", f64::from(i32::try_from(i - 32).expect("in range")) / 100.0))).collect()
}

/// A quiet zero-straddling region of tenths with a contiguous burst of billion-scale values: the
/// mixed-magnitude mantissa stream per-block adaptive packing exists for.
fn v_scaled_blocked(rows: usize) -> Vec<BigDecimal> {
	(0..rows)
		.map(|i| {
			if (64..128).contains(&i) {
				// Tenths, never halves: `x.5` is binary-exact and would pull the column to F64.
				let sign = if i % 2 == 0 { "" } else { "-" };
				dec(&format!("{sign}1000000000.{}", i % 7 + 1))
			} else {
				dec(&format!("{}.{:01}", i64::try_from(i % 5).expect("small") - 2, i % 7 + 1))
			}
		})
		.collect()
}

/// A high base with a small jitter: Frame-of-Reference subtracts each block's minimum and packs
/// only the jitter, which is the whole idea.
fn v_scaled_for(rows: usize) -> Vec<BigDecimal> {
	(0..rows).map(|i| dec(&format!("1000000000.{:02}", i % 7))).collect()
}

/// Nineteen decimal places: past the scale-18 ceiling a `ScaledI64` mantissa can carry.
fn v_scaled_i128(rows: usize) -> Vec<BigDecimal> {
	(0..rows).map(|i| dec(&format!("1.123456789012345{i:04}"))).collect()
}

/// Twenty-eight significant digits: no scaled integer holds the mantissa, `Decimal128` does.
fn v_decimal128(rows: usize) -> Vec<BigDecimal> {
	(0..rows).map(|i| dec(&format!("123456789012345678901234.5678{i:02}"))).collect()
}

/// Forty digits with a fraction: `ScaledI128` overflows and `Decimal128` is lossy, so the exact
/// text backstop is the only encoding left.
fn v_text(rows: usize) -> Vec<BigDecimal> {
	(0..rows).map(|i| dec(&format!("1{}0.5{i:02}", "0".repeat(38)))).collect()
}

// ---------------------------------------------------------------------------------------------
// The cases.
// ---------------------------------------------------------------------------------------------

/// A frame's bytes, and the rows they are the encoding of.
struct Built {
	bytes: Vec<u8>,
	timestamps: Vec<i64>,
	values: Vec<Option<BigDecimal>>,
}

/// Build a single-block (v5) frame from a dense value column.
fn single_block(stamps: &[i64], values: &[BigDecimal], unit: TimeUnit, opts: &FrameOptions) -> Built {
	let segment = Segment::build(stamps, values, unit, &exact()).expect("a frame always builds: the text encoding is the backstop");
	Built { bytes: write_segment_with(&segment, opts), timestamps: stamps.to_vec(), values: values.iter().map(|value| Some(value.clone())).collect() }
}

/// Build a single-block (v5) frame from a nullable value column — the shape that puts a
/// [`weft_physical_type::NullMask`] on disk.
fn single_block_nullable(stamps: &[i64], values: &[Option<BigDecimal>], unit: TimeUnit) -> Built {
	let segment = Segment::build_nullable(stamps, values, unit, &exact()).expect("a frame always builds");
	Built { bytes: write_segment_with(&segment, &FrameOptions::DEFAULT), timestamps: stamps.to_vec(), values: values.to_vec() }
}

/// Build a paged (v6) frame.
fn paged(stamps: &[i64], values: &[BigDecimal], unit: TimeUnit, rows_per_page: usize, stride: Option<usize>) -> Built {
	let segment = PagedSegment::build(stamps, values, unit, &exact(), rows_per_page).expect("a paged frame always builds");
	let bytes = match stride {
		Some(stride) => segment.write_to_checkpointed(stride),
		None => segment.write_to(),
	};
	let (timestamps, values) = segment.decode_nullable();
	Built { bytes, timestamps, values }
}

/// Wrap a v5 frame in the v7 envelope.
fn envelope(inner: Built, extensions: Vec<FrameExtension>) -> Built {
	let bytes = wrap_frame_envelope(&inner.bytes, &extensions).expect("a known body version always wraps");
	Built { bytes, timestamps: inner.timestamps, values: inner.values }
}

/// The layout a case's bytes must be read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
	/// A v5 single-block body, wrapped or not.
	SingleBlock,
	/// A v6 paged body, wrapped or not.
	Paged,
}

/// One committed fixture: how it is written, and what reading it back must say.
struct Case {
	/// The file under `tests/golden/`.
	fixture: &'static str,
	/// What the case exists to pin, quoted in the failure message so a red run explains itself.
	pins: &'static str,
	/// The version in the frame's own header.
	version: u16,
	/// The layout the registry resolves for the body.
	layout: FrameLayout,
	/// Which reader takes these bytes.
	mode: Mode,
	/// The value-codec label the frame's block byte reports, or `None` for a layout whose value
	/// block is not a single column (a paged frame reports one codec per page, not one per frame).
	value_codec: Option<&'static str>,
	/// How the bytes are produced.
	build: fn() -> Built,
}

/// The committed fixture's bytes, or a fresh recording of them while goldens are being rewritten.
///
/// A recording run deletes and rewrites the files, and the four tests run in parallel, so a reader
/// that insisted on the file would fail for a reason that has nothing to do with the format.
fn fixture_bytes(case: &Case) -> Vec<u8> {
	let path = golden_dir().join(case.fixture);
	match std::fs::read(&path) {
		Ok(bytes) => bytes,
		Err(_) if updating() => (case.build)().bytes,
		Err(error) => panic!("{}: cannot read the committed fixture: {error}", path.display()),
	}
}

/// Decode a fixture's rows, whichever layout and wrapping it uses.
fn decode(case: &Case, bytes: &[u8]) -> (Vec<i64>, Vec<Option<BigDecimal>>) {
	match case.mode {
		Mode::SingleBlock => read_frame_segment(bytes).expect("the committed frame must read").decode_nullable(),
		Mode::Paged => read_frame_paged_segment(bytes).expect("the committed paged frame must read").decode_nullable(),
	}
}

fn default_opts() -> FrameOptions {
	FrameOptions::DEFAULT
}

// ---- single-block body: one case per physical type -------------------------------------------

fn case_f32() -> Built {
	single_block(&ts_regular(64), &v_f32(64), TimeUnit::Millis, &default_opts())
}

fn case_f64() -> Built {
	single_block(&ts_regular(64), &v_f64(64), TimeUnit::Millis, &default_opts())
}

fn case_scaled_bitpack() -> Built {
	single_block(&ts_regular(64), &v_scaled_bitpack(64), TimeUnit::Millis, &default_opts())
}

fn case_scaled_blocked() -> Built {
	single_block(&ts_regular(192), &v_scaled_blocked(192), TimeUnit::Millis, &default_opts())
}

fn case_scaled_for() -> Built {
	single_block(&ts_regular(192), &v_scaled_for(192), TimeUnit::Millis, &default_opts())
}

fn case_scaled_i128() -> Built {
	single_block(&ts_regular(64), &v_scaled_i128(64), TimeUnit::Millis, &default_opts())
}

fn case_decimal128() -> Built {
	single_block(&ts_regular(64), &v_decimal128(64), TimeUnit::Millis, &default_opts())
}

fn case_text() -> Built {
	single_block(&ts_regular(64), &v_text(64), TimeUnit::Millis, &default_opts())
}

fn case_nulls() -> Built {
	let stamps = ts_regular(96);
	let dense = v_scaled_for(96);
	// Every eighth row absent, and one run of three: a dense mask, a sparse mask, and a run
	// boundary are three different byte shapes, and a fixture should contain all three.
	let values: Vec<Option<BigDecimal>> = dense.into_iter().enumerate().map(|(i, value)| if i % 8 == 7 { None } else { Some(value) }).collect();
	single_block_nullable(&stamps, &values, TimeUnit::Millis)
}

// ---- single-block body: one case per timestamp codec -----------------------------------------

fn case_ts_plain() -> Built {
	single_block(&ts_plain(), &v_scaled_for(ts_plain().len()), TimeUnit::Millis, &default_opts())
}

fn case_ts_bitpack() -> Built {
	let stamps = ts_bitpack(192);
	let values = v_scaled_for(stamps.len());
	single_block(&stamps, &values, TimeUnit::Millis, &default_opts())
}

fn case_ts_rle() -> Built {
	let stamps = ts_rle(200);
	let values = v_scaled_for(stamps.len());
	single_block(&stamps, &values, TimeUnit::Millis, &default_opts())
}

fn case_ts_gorilla() -> Built {
	let stamps = ts_gorilla(256);
	let values = v_scaled_for(stamps.len());
	single_block(&stamps, &values, TimeUnit::Millis, &default_opts())
}

fn case_ts_blocked() -> Built {
	let stamps = ts_blocked(256);
	let values = v_scaled_for(stamps.len());
	single_block(&stamps, &values, TimeUnit::Millis, &default_opts())
}

// ---- single-block body: the two opt-in layouts ----------------------------------------------

fn case_checkpointed() -> Built {
	let opts = FrameOptions { checkpoint_stride: Some(8), transposed_max_overhead: None };
	single_block(&ts_regular(192), &v_scaled_for(192), TimeUnit::Millis, &opts)
}

/// The bit-sliced value codec, which a default build can neither write nor read. Compiled only
/// with `bitsliced-codec`; the CI `features` job runs this suite with `--all-features`, and that
/// run is the only thing that pins these bytes.
#[cfg(feature = "bitsliced-codec")]
fn case_transposed() -> Built {
	let opts = FrameOptions { checkpoint_stride: None, transposed_max_overhead: Some(3.0) };
	single_block(&ts_regular(64), &v_scaled_bitpack(64), TimeUnit::Millis, &opts)
}

// ---- other layouts --------------------------------------------------------------------------

fn case_paged() -> Built {
	paged(&ts_regular(96), &v_scaled_for(96), TimeUnit::Millis, 32, None)
}

fn case_paged_checkpointed() -> Built {
	paged(&ts_regular(96), &v_scaled_for(96), TimeUnit::Millis, 32, Some(8))
}

fn case_envelope_bare() -> Built {
	envelope(case_scaled_bitpack(), Vec::new())
}

/// An envelope carrying a non-critical extension this build has never heard of — the mechanism
/// X-FRAME exists for: a later writer adds a block in front of a frozen body, and every reader
/// that knows the envelope skips what it does not know instead of refusing the file.
fn case_envelope_unknown_extension() -> Built {
	let extensions = vec![FrameExtension { id: 4_242, critical: false, payload: b"a payload the envelope never interprets".to_vec() }];
	envelope(case_scaled_bitpack(), extensions)
}

/// Every committed frame fixture.
const CASES: &[Case] = &[Case { fixture: "frame_v5_f32_varint.bin", pins: "the F32 physical type and its plain per-value payload", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("varint"), build: case_f32 }, Case { fixture: "frame_v5_f64_varint.bin", pins: "the F64 physical type: exact in binary64, out of reach of binary32", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("varint"), build: case_f64 }, Case { fixture: "frame_v5_scaledi64_bitpack.bin", pins: "ScaledI64 with the global fixed-width value codec", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_bitpack"), build: case_scaled_bitpack }, Case { fixture: "frame_v5_scaledi64_blocked.bin", pins: "ScaledI64 with the per-block adaptive value codec (mixed-magnitude mantissas)", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_blocked"), build: case_scaled_blocked }, Case { fixture: "frame_v5_scaledi64_for.bin", pins: "ScaledI64 with the Frame-of-Reference value codec (clustered high base)", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_scaled_for }, Case { fixture: "frame_v5_scaledi128_varint.bin", pins: "the ScaledI128 physical type: a scale no 64-bit mantissa can carry", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("varint"), build: case_scaled_i128 }, Case { fixture: "frame_v5_decimal128_varint.bin", pins: "the Decimal128 physical type", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("varint"), build: case_decimal128 }, Case { fixture: "frame_v5_bigdecimaltext_varint.bin", pins: "the exact text backstop: the encoding that is always available and never lossy", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("varint"), build: case_text }, Case { fixture: "frame_v5_null_mask.bin", pins: "the null mask, dense and sparse runs in one column", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_nulls }, Case { fixture: "frame_v5_ts_plain_varint.bin", pins: "the plain delta-of-delta timestamp block, the floor every other codec optimises over", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_ts_plain }, Case { fixture: "frame_v5_ts_bitpack.bin", pins: "the bit-packed timestamp block (slowly drifting deltas: narrow globally, no run to encode)", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_ts_bitpack }, Case { fixture: "frame_v5_ts_rle.bin", pins: "the run-length timestamp block (a constant second difference)", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_ts_rle }, Case { fixture: "frame_v5_ts_gorilla.bin", pins: "the Gorilla bucket-prefix timestamp block (scattered small second differences)", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_ts_gorilla }, Case { fixture: "frame_v5_ts_blocked.bin", pins: "the per-block adaptive timestamp block (narrow jitter, one wide window)", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_ts_blocked }, Case { fixture: "frame_v5_checkpointed.bin", pins: "the persisted sparse checkpoint index (stride 8)", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_for"), build: case_checkpointed }, Case { fixture: "frame_v6_paged.bin", pins: "the paged layout: three pages, the page table, and per-page pruning metadata", version: FRAME_PAGED, layout: FrameLayout::Paged, mode: Mode::Paged, value_codec: None, build: case_paged }, Case { fixture: "frame_v6_paged_checkpointed.bin", pins: "the paged layout with a per-page checkpoint index", version: FRAME_PAGED, layout: FrameLayout::Paged, mode: Mode::Paged, value_codec: None, build: case_paged_checkpointed }, Case { fixture: "frame_v7_envelope_bare.bin", pins: "the envelope with an empty extension table around a v5 body", version: FRAME_ENVELOPE, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_bitpack"), build: case_envelope_bare }, Case { fixture: "frame_v7_envelope_unknown_noncritical.bin", pins: "an envelope carrying extension 4242, which no reader of this build knows", version: FRAME_ENVELOPE, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_bitpack"), build: case_envelope_unknown_extension }];

/// The gated codec, appended to the table only when this build can write it.
#[cfg(feature = "bitsliced-codec")]
const CASES_TRANSPOSED: &[Case] = &[Case { fixture: "frame_v5_transposed.bin", pins: "the bit-sliced (bit-plane-major) value codec, VAL_CODEC_TRANSPOSED", version: FRAME_SINGLE_BLOCK, layout: FrameLayout::SingleBlock, mode: Mode::SingleBlock, value_codec: Some("scaled_transposed"), build: case_transposed }];

/// Every case this build can write.
fn cases() -> Vec<&'static Case> {
	// Written as a chain rather than `let mut` + `extend`, because the gated half is the only thing
	// that needs the binding mutable and a default build would then fail an unused-`mut` denial.
	#[cfg(feature = "bitsliced-codec")]
	let gated: &[Case] = CASES_TRANSPOSED;
	#[cfg(not(feature = "bitsliced-codec"))]
	let gated: &[Case] = &[];
	CASES.iter().chain(gated.iter()).collect()
}

#[test]
fn golden_frames_match_their_committed_bytes() {
	for case in cases() {
		let built = (case.build)();
		check_golden(case.fixture, &built.bytes);
	}
}

#[test]
fn golden_frames_describe_decode_and_name_their_codec() {
	for case in cases() {
		let bytes = fixture_bytes(case);
		let built = (case.build)();

		// What the frame says it is, before anything is decoded.
		let info = describe_frame(&bytes).unwrap_or_else(|fault| panic!("{}: the committed fixture is not a readable frame: {fault:?}", case.fixture));
		assert_eq!(info.version, case.version, "{}: {pins}", case.fixture, pins = case.pins);
		assert_eq!(info.layout, Some(case.layout), "{}: the registry must resolve this body to {:?}", case.fixture, case.layout);
		assert_eq!(frame_layout_of(&bytes).expect("a committed fixture is a known frame"), case.layout, "{}: `frame_layout_of` disagrees with `describe_frame`", case.fixture);

		// The codec label, read out of the frame rather than trusted from the writer.
		if let Some(expected) = case.value_codec {
			// The codec label lives in the body, so an envelope is opened before it is read.
			let body = if case.version == FRAME_ENVELOPE { unwrap_frame_envelope(&bytes).expect("a committed envelope must unwrap") } else { &bytes[..] };
			let actual = frame_value_codec(body).unwrap_or_else(|error| panic!("{}: no value codec label: {error:?}", case.fixture));
			assert_eq!(actual, expected, "{}: {pins} — the writer chose `{actual}`, which is a format event: the shape of every file this column shape produces has changed", case.fixture, pins = case.pins);
		}

		// The envelope's own promise, pinned across two fixtures rather than against itself: the
		// body inside a v7 file is byte-for-byte the committed v5 file, which is what "a reader
		// that has never heard of the envelope, pointed at the body, still reads the file it has
		// always read" means in bytes. Both envelope cases wrap that corpus; if one of them ever
		// wraps something else the lookup below fails loudly instead of quietly comparing an
		// envelope with itself.
		if case.version == FRAME_ENVELOPE {
			let body = unwrap_frame_envelope(&bytes).expect("a non-critical extension never blocks a reader");
			let wrapped = cases().into_iter().find(|other| other.fixture == WRAPPED_FIXTURE).unwrap_or_else(|| panic!("{}: wraps {WRAPPED_FIXTURE}, which is no longer a case in this suite", case.fixture));
			assert_eq!(body, (wrapped.build)().bytes.as_slice(), "{}: the envelope must carry a freshly written {WRAPPED_FIXTURE} unmodified", case.fixture);
			assert_eq!(body, fixture_bytes(wrapped).as_slice(), "{}: the envelope's body is not the committed {WRAPPED_FIXTURE}: one of the two fixtures was re-recorded without the other", case.fixture);
		}

		// And the meaning: the fixture decodes to the rows it was written from.
		let (timestamps, values) = decode(case, &bytes);
		assert_eq!(timestamps, built.timestamps, "{}: the decoded timestamps are not the rows written", case.fixture);
		assert_eq!(values.len(), built.values.len(), "{}: the decoded row count is not the rows written", case.fixture);
		for (index, (actual, expected)) in values.iter().zip(&built.values).enumerate() {
			assert_eq!(actual.as_ref().map(bigdecimal::BigDecimal::to_plain_string), expected.as_ref().map(bigdecimal::BigDecimal::to_plain_string), "{}: row {index} decodes to a different value", case.fixture);
		}
	}
}

/// The fixtures a build without the gate cannot write, named here rather than `cfg`'d so the list
/// cannot silently disagree with the codec it names.
const GATED_FIXTURES: &[&str] = &["frame_v5_transposed.bin"];

/// The single-block fixture both envelope cases wrap.
const WRAPPED_FIXTURE: &str = "frame_v5_scaledi64_bitpack.bin";

/// A fixture nobody names is a fixture nobody maintains: it drifts, and a red run cannot say what
/// it was protecting.
#[test]
fn every_committed_fixture_is_named_by_a_case() {
	if updating() {
		// A recording run has not necessarily written every file yet when this test is scheduled.
		return;
	}
	let expected: Vec<&str> = cases().iter().map(|case| case.fixture).collect();
	let dir = golden_dir();
	// This suite owns the `frame_` namespace of the shared golden directory; `enc_` belongs to
	// `golden_codecs.rs`, whose fixtures are not frames and must not be judged by a frame test.
	let entries: Vec<String> = std::fs::read_dir(&dir).expect("tests/golden must exist and be committed").filter_map(|entry| entry.ok().map(|entry| entry.file_name().to_string_lossy().into_owned())).filter(|name| name.starts_with("frame_") && name.ends_with(".bin")).collect();
	for name in &entries {
		// The all-features run owns a gated fixture's bytes; a default build neither writes nor
		// reads it, so its presence in the directory is the gate working, not an orphan.
		if GATED_FIXTURES.contains(&name.as_str()) && !cfg!(feature = "bitsliced-codec") {
			continue;
		}
		assert!(expected.contains(&name.as_str()), "{} contains {name}, which no case in this suite writes; delete it or add its case", dir.display());
	}
	for case in cases() {
		assert!(entries.iter().any(|name| name == case.fixture), "{} is named by a case but was never committed; run the suite with WEFT_UPDATE_GOLDENS=1", case.fixture);
	}
}

/// The fixtures are the *frames*, not a copy of a corpus: pin that a byte change anywhere in a
/// fixture is caught, by changing one byte of a decoded copy of each frame's trailer.
#[test]
fn a_truncated_fixture_is_refused_not_misread() {
	for case in cases() {
		let bytes = fixture_bytes(case);
		let truncated = &bytes[..bytes.len() - 1];
		let outcome = match case.mode {
			Mode::SingleBlock => read_frame_segment(truncated).map(|_| ()),
			Mode::Paged => read_frame_paged_segment(truncated).map(|_| ()),
		};
		assert!(outcome.is_err(), "{}: a frame one byte short of its checksum must be refused, not decoded ({:?})", case.fixture, case.pins);
	}
}
