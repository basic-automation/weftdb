//! ROB-9: the mutation harness for `.weftseg` frames.
//!
//! A CRC is an integrity check, not a signature (DECISIONS 2026-10-30, ROB-8): a hostile or
//! damaged-then-rewritten file carries a correct one, so every byte behind it must be safe to
//! decode. Flipping bytes of a frame and handing it to a reader only exercises the CRC; this
//! harness flips bytes and then **reseals** the frame — recomputes its trailer, and for a v7
//! envelope the wrapped frame's trailer too — so each mutation reaches the decoders proper.
//!
//! Every committed golden frame (`tests/golden/frame_*.bin`, every layout and codec the writer
//! produces) is mutated systematically — five byte substitutions at every offset, and every
//! truncation — and by a seeded random corpus of multi-byte mutations. Each mutant goes through
//! every public frame reader. The contract is the one ROB-8 states: a reader answers `Ok` or a
//! typed `WeftSegError` that [`frame_fault_of`] classifies, and **never panics**; a frame a reader
//! accepts must also decode without panicking.
#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

#[path = "golden/harness.rs"]
mod harness;

use std::panic::{catch_unwind, AssertUnwindSafe};

use harness::golden_dir;
use weft_physical_type::{crc32, describe_frame, frame_fault_of, frame_version, read_frame_paged_segment, read_frame_point, read_frame_points, read_frame_range, read_frame_range_physical, read_frame_segment, unwrap_frame_envelope, FRAME_ENVELOPE};

/// The byte substitutions applied at every offset: two single-bit flips (low and high, the
/// varint continuation bit among them), and three values that are boundaries for lengths,
/// counts and tags.
/// One named byte substitution.
type Substitution = (&'static str, fn(u8) -> u8);

const SUBSTITUTIONS: [Substitution; 5] = [("xor 0x01", |b| b ^ 0x01), ("xor 0x80", |b| b ^ 0x80), ("set 0x00", |_| 0x00), ("set 0x7f", |_| 0x7f), ("set 0xff", |_| 0xff)];

/// Random multi-byte mutants per golden.
const RANDOM_MUTANTS: usize = 512;

/// One golden frame and, for a v7 envelope, where its wrapped frame sits.
struct Golden {
	name: String,
	bytes: Vec<u8>,
	/// The wrapped frame's byte range inside a v7 envelope (its own trailer included).
	inner: Option<std::ops::Range<usize>>,
}

fn goldens() -> Vec<Golden> {
	let mut out = Vec::new();
	let mut entries: Vec<_> = std::fs::read_dir(golden_dir()).expect("the golden directory").filter_map(Result::ok).map(|entry| entry.path()).filter(|path| path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("frame_")) && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))).collect();
	entries.sort();
	for path in entries {
		let bytes = std::fs::read(&path).expect("a readable golden");
		let inner = (frame_version(&bytes).ok() == Some(FRAME_ENVELOPE)).then(|| {
			let wrapped = unwrap_frame_envelope(&bytes).expect("a committed envelope unwraps");
			let start = bytes.len() - 4 - wrapped.len();
			start..start + wrapped.len()
		});
		out.push(Golden { name: path.file_name().expect("a file name").to_string_lossy().into_owned(), bytes, inner });
	}
	assert!(out.len() >= 20, "the harness covers every committed frame golden; found {}", out.len());
	out
}

/// Recompute a frame's CRC-32 trailer over everything before it.
fn reseal_trailer(frame: &mut [u8]) {
	if frame.len() < 4 {
		return;
	}
	let body = frame.len() - 4;
	let crc = crc32(&frame[..body]);
	frame[body..].copy_from_slice(&crc.to_le_bytes());
}

/// Reseal a mutant so the mutation reaches the decoders: the wrapped frame's trailer first
/// (when the mutant still has the original's length, so the wrapped range is where it was),
/// then the outer one.
fn reseal(golden: &Golden, mutant: &mut [u8]) {
	if let Some(inner) = golden.inner.as_ref().filter(|_| mutant.len() == golden.bytes.len()) {
		reseal_trailer(&mut mutant[inner.clone()]);
	}
	reseal_trailer(mutant);
}

/// Run every public reader over `bytes`; a panic anywhere is returned as its message.
fn drive(bytes: &[u8]) -> Result<(), String> {
	let outcome = catch_unwind(AssertUnwindSafe(|| {
		let _ = describe_frame(bytes);
		match read_frame_segment(bytes) {
			Ok(segment) => {
				let _ = segment.decode_nullable();
			}
			Err(error) => {
				let _ = frame_fault_of(&error);
			}
		}
		match read_frame_paged_segment(bytes) {
			Ok(paged) => {
				let _ = paged.decode_nullable();
				let _ = paged.read_time_range(i64::MIN, i64::MAX);
			}
			Err(error) => {
				let _ = frame_fault_of(&error);
			}
		}
		let _ = read_frame_point(bytes, 0);
		let _ = read_frame_point(bytes, 1_700_000_000);
		let _ = read_frame_points(bytes, &[i64::MIN, 0, 1, 1_700_000_010, i64::MAX]);
		let _ = read_frame_range(bytes, i64::MIN, i64::MAX);
		let _ = read_frame_range(bytes, 0, 1_700_000_050);
		let _ = read_frame_range_physical(bytes, i64::MIN, i64::MAX);
	}));
	outcome.map_err(|panic| panic.downcast_ref::<&str>().map(ToString::to_string).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "a non-string panic".to_string()))
}

/// `SplitMix64`: a seeded, dependency-free generator, so a failing mutant is reproducible
/// from the seed printed beside it.
struct SplitMix(u64);

impl SplitMix {
	const fn next(&mut self) -> u64 {
		self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
		let mut z = self.0;
		z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
		z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
		z ^ (z >> 31)
	}

	fn below(&mut self, bound: usize) -> usize {
		usize::try_from(self.next() % u64::try_from(bound).expect("usize fits u64")).expect("below a usize bound")
	}
}

/// Collect every failure, then report them together (the first few in full), so one run names
/// the whole set rather than the first.
fn assert_no_failures(failures: &[String], mutants: usize) {
	assert!(failures.is_empty(), "{} of {mutants} resealed mutants panicked a frame reader; first ones:\n{}", failures.len(), failures.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn the_unmutated_goldens_read() {
	for golden in goldens() {
		assert_eq!(drive(&golden.bytes), Ok(()), "{}", golden.name);
		assert!(describe_frame(&golden.bytes).is_ok(), "{} is a committed, valid frame", golden.name);
	}
}

#[test]
fn every_byte_substitution_is_refused_or_read_without_a_panic() {
	let previous = std::panic::take_hook();
	std::panic::set_hook(Box::new(|_| {}));
	let mut failures = Vec::new();
	let mut mutants = 0;
	for golden in goldens() {
		// The trailer itself is what `reseal` rewrites, so the offsets that matter are the body's.
		for offset in 0..golden.bytes.len().saturating_sub(4) {
			for (label, substitute) in SUBSTITUTIONS {
				let mut mutant = golden.bytes.clone();
				let replaced = substitute(mutant[offset]);
				if replaced == mutant[offset] {
					continue;
				}
				mutant[offset] = replaced;
				reseal(&golden, &mut mutant);
				mutants += 1;
				if let Err(panic) = drive(&mutant) {
					failures.push(format!("{} offset {offset} {label}: {panic}", golden.name));
				}
			}
		}
	}
	std::panic::set_hook(previous);
	assert_no_failures(&failures, mutants);
}

#[test]
fn every_truncation_is_refused_without_a_panic() {
	let previous = std::panic::take_hook();
	std::panic::set_hook(Box::new(|_| {}));
	let mut failures = Vec::new();
	let mut mutants = 0;
	for golden in goldens() {
		for len in 0..golden.bytes.len() {
			// Both forms: the cut as it lies on disk, and the cut resealed so it passes the CRC.
			let mut cut = golden.bytes[..len].to_vec();
			for resealed in [false, true] {
				if resealed {
					reseal_trailer(&mut cut);
				}
				mutants += 1;
				if let Err(panic) = drive(&cut) {
					failures.push(format!("{} truncated to {len} (resealed: {resealed}): {panic}", golden.name));
				}
			}
		}
	}
	std::panic::set_hook(previous);
	assert_no_failures(&failures, mutants);
}

#[test]
fn seeded_multi_byte_mutants_are_refused_or_read_without_a_panic() {
	let previous = std::panic::take_hook();
	std::panic::set_hook(Box::new(|_| {}));
	let mut failures = Vec::new();
	let mut mutants = 0;
	for (index, golden) in goldens().iter().enumerate() {
		let body = golden.bytes.len().saturating_sub(4);
		if body == 0 {
			continue;
		}
		for case in 0..RANDOM_MUTANTS {
			let seed = (u64::try_from(index).expect("fits") << 32) | u64::try_from(case).expect("fits");
			let mut rng = SplitMix(seed);
			let mut mutant = golden.bytes.clone();
			for _ in 0..=rng.below(8) {
				let offset = rng.below(body);
				mutant[offset] = u8::try_from(rng.next() & 0xff).expect("masked to a byte");
			}
			reseal(golden, &mut mutant);
			mutants += 1;
			if let Err(panic) = drive(&mutant) {
				failures.push(format!("{} seed {seed:#x}: {panic}", golden.name));
			}
		}
	}
	std::panic::set_hook(previous);
	assert_no_failures(&failures, mutants);
}
