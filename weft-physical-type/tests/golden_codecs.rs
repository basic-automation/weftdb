//! The codecs behind the frame, pinned at the codec boundary (roadmap slice **FRE-3**).
//!
//! [`golden_frames`](golden_frames) pins what the `.weftseg` writer actually puts on disk. That set
//! has a blind spot by construction: the writer chooses **one** codec per column, so a codec it does
//! not choose can change its output shape — a different run split, a wider bit width, a different
//! leading-byte rule — and no frame fixture notices. These goldens close that gap by pinning each
//! codec directly, on the corpus where that codec is the interesting one, so every codec in the crate
//! has a byte- or decision-level pin whether or not the selector currently likes it.
//!
//! Two kinds of pin appear, and the difference is deliberate:
//!
//! * A **byte golden** (`.bin`) for a codec that produces a stream on its own — the bit packer, and
//!   the advisory `f64` family under `experimental-codecs`. Assertion is byte-for-byte.
//! * A **decision golden** (`.txt`) for a codec whose *contract* is a choice rather than a stream:
//!   how a run-length split falls, what delta-of-delta leaves behind, which `f64` codec wins a corpus.
//!   Those choices decide the bytes a future writer will emit, so they are pinned as rendering text
//!   a reviewer can read.
//!
//! All corpora are exact-arithmetic (`i64`, or `f64` values that are multiples of 1/8): no transcendental
//! function, no wall clock, no hash ordering. A golden that is not bit-reproducible on the next
//! machine is a flaky test with a scary name.
//!
//! Regenerating is `WEFT_UPDATE_GOLDENS=1`, and it is a codec change: see
//! [`golden::harness`](harness).

#[path = "golden/harness.rs"]
mod harness;

use std::fmt::Write as _;

use harness::{check_golden, check_golden_text, updating};
use weft_physical_type::{bitpack_encode, bitpack_width, encode_delta, encode_delta_of_delta, rle_encode, zigzag_varint_bytes, DeltaColumn, DeltaOfDeltaColumn, TimeUnit};

/// The unit the timestamp corpora are expressed in; irrelevant to the codecs' shapes, load-bearing
/// for the rendered goldens (it is part of what a reader has to know).
const UNIT: TimeUnit = TimeUnit::Millis;

// ---------------------------------------------------------------------------------------------
// Corpora. Named for the property they exercise, since that property is the reason a codec output
// has the shape it has.
// ---------------------------------------------------------------------------------------------

/// `0..n`, the perfectly regular series: every second difference is zero.
fn regular(n: i64) -> Vec<i64> {
	(0..n).collect()
}

/// Regular at a 1000 ms period, then one 10-minute gap: the shape a real collector makes when it
/// loses a batch. A codec that handles this by widening every value is worse than one that pays for
/// the outlier alone.
fn regular_with_gap() -> Vec<i64> {
	let mut out = (0..64_i64).map(|i| i * 1000).collect::<Vec<_>>();
	out.push(64 * 1000 + 600_000);
	out.extend((65..96_i64).map(|i| 64 * 1000 + 600_000 + (i - 64) * 1000));
	out
}

/// Every value identical: the run-length best case and the bit-packer's zero-width case.
fn constant() -> Vec<i64> {
	vec![1_700_000_000_000; 200]
}

/// Two values alternating, so RLE's runs are all length one — the worst case, pinned so a change to
/// the run-merge heuristic cannot quietly turn 200 runs into 200 pairs of varints without being seen.
fn alternating() -> Vec<i64> {
	(0..200_i64).map(|i| if i % 2 == 0 { 1_700_000_000_000 } else { 1_700_000_000_001 }).collect()
}

/// A deterministic linear congruential shuffle: no two adjacent values related, which is the case
/// every delta scheme loses on and the one a store must still survive.
fn scattered() -> Vec<i64> {
	let mut state = 0x2545_F491_4F6C_DD1D_u64;
	(0..128_i64)
		.map(|_| {
			state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
			(state >> 24) as i64 % 1_000_000
		})
		.collect()
}

/// One value at every bit width that matters: the boundaries where the packer changes word layout.
fn widths() -> Vec<i64> {
	vec![0, 1, 127, 128, 32_767, 32_768, 2_147_483_647, 2_147_483_648, 9_223_372_036_854_775_806, i64::MAX]
}

/// `(name, values)` for every corpus this suite walks.
fn corpora() -> Vec<(&'static str, Vec<i64>)> {
	vec![("regular_64", regular(64)), ("regular_gap", regular_with_gap()), ("constant_200", constant()), ("alternating_200", alternating()), ("scattered_128", scattered()), ("widths", widths()), ("empty", Vec::new()), ("single", vec![42])]
}

// ---------------------------------------------------------------------------------------------
// Byte goldens: the bit packer.
// ---------------------------------------------------------------------------------------------

#[test]
fn bitpack_goldens_pin_width_and_payload() {
	for (name, values) in corpora() {
		let (fresh_width, fresh_payload) = bitpack_encode(&values);
		// The width is part of the format: a reader that trusts a stale width misreads the payload as
		// garbage rather than as an error, so it is prefixed into the golden and pinned with the bytes.
		let mut fresh = fresh_width.to_le_bytes().to_vec();
		fresh.extend_from_slice(&fresh_payload);
		check_golden(&format!("enc_ts_bitpack_{name}.bin"), &fresh);
		assert_eq!(fresh_width, bitpack_width(&values), "{name}: bitpack_encode and bitpack_width disagree on the width");
		assert_eq!(fresh_payload.len(), (values.len() * fresh_width as usize).div_ceil(8), "{name}: {} values at width {fresh_width} do not pack to a bit-count payload", values.len());
	}
}

// ---------------------------------------------------------------------------------------------
// Decision goldens: run-split, delta, delta-of-delta, varint sizing.
// ---------------------------------------------------------------------------------------------

/// Renders `(value, run_length)` pairs, one per line.
fn render_runs(values: &[i64]) -> String {
	let mut out = String::new();
	let runs = rle_encode(values);
	writeln!(out, "runs={}", runs.len()).expect("writing to a String cannot fail");
	for (value, length) in runs {
		writeln!(out, "{value}\t{length}").expect("writing to a String cannot fail");
	}
	out
}

/// Renders a delta column: the anchor, then one first-order difference per line.
fn render_delta(column: &DeltaColumn) -> String {
	let mut out = String::new();
	writeln!(out, "unit={}\tfirst={}\tdeltas={}", column.unit.name(), column.first, column.deltas.len()).expect("writing to a String cannot fail");
	for delta in &column.deltas {
		writeln!(out, "{delta}").expect("writing to a String cannot fail");
	}
	out
}

/// Renders a delta-of-delta column, including the anchor and first delta a reader needs to resume.
fn render_delta_of_delta(column: &DeltaOfDeltaColumn) -> String {
	let mut out = String::new();
	writeln!(out, "unit={}\tfirst={}\tfirst_delta={:?}\tdods={}", column.unit.name(), column.first, column.first_delta, column.dods.len()).expect("writing to a String cannot fail");
	for dod in &column.dods {
		writeln!(out, "{dod}").expect("writing to a String cannot fail");
	}
	out
}

#[test]
fn rle_goldens_pin_the_run_split() {
	let mut fresh = String::new();
	for (name, values) in corpora() {
		fresh.push_str(&format!("# {name}\n{}\n", render_runs(&values)));
	}
	check_golden_text("enc_ts_rle.txt", &fresh);
}

#[test]
fn delta_goldens_pin_the_anchors_and_differences() {
	let mut fresh = String::new();
	for (name, values) in corpora() {
		fresh.push_str(&format!("# {name} delta\n{}\n# {name} delta_of_delta\n{}\n", render_delta(&encode_delta(&values, UNIT)), render_delta_of_delta(&encode_delta_of_delta(&values, UNIT))));
	}
	check_golden_text("enc_ts_delta.txt", &fresh);
}

#[test]
fn zigzag_goldens_pin_the_varint_sizing() {
	// `zigzag_varint_bytes` is what a blocked timestamp plan pays per value, so a change here is a
	// change to every file the blocked codec writes — including the frame fixture that pins it.
	let mut fresh = String::new();
	for (name, values) in corpora() {
		writeln!(fresh, "{name}\ttotal={}\tmean={:.3}", zigzag_varint_bytes(&values), if values.is_empty() { 0.0 } else { zigzag_varint_bytes(&values) as f64 / values.len() as f64 }).expect("writing to a String cannot fail");
	}
	check_golden_text("enc_ts_zigzag.txt", &fresh);
}

// ---------------------------------------------------------------------------------------------
// The advisory f64 family: gated behind `experimental-codecs`, exactly like the module it pins.
// ---------------------------------------------------------------------------------------------

/// The `f64` corpora. Every value is an exact multiple of 1/8, so the input bits are identical on
/// every platform and every Rust version — the codecs key on leading and trailing zero bits, so an
/// input that rounds differently anywhere would make these goldens platform-dependent.
#[cfg(feature = "experimental-codecs")]
fn f64_corpora() -> Vec<(&'static str, Vec<f64>)> {
	let eighths = |offset: i64, step: i64, n: usize| -> Vec<f64> { (0..n).map(|i| (offset + step * i as i64) as f64 / 8.0).collect() };
	let mut scattered = Vec::new();
	let mut state = 0x9E37_79B9_7F4A_7C15_u64;
	for _ in 0..128 {
		state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
		scattered.push((state % 1_000_000) as f64 / 8.0);
	}
	vec![("fine_ramp_128", eighths(0, 1, 128)), ("coarse_ramp_128", eighths(1_000_000, 512, 128)), ("constant_200", vec![42.25; 200]), ("alternating_200", (0..200).map(|i| if i % 2 == 0 { 42.25 } else { -42.25 }).collect()), ("scattered_128", scattered), ("empty", Vec::new()), ("single", vec![7.0])]
}

/// Every codec in the gated module, as (name, encoder). `elf_f64_encode` is `Option` because it
/// declines some inputs; the golden pins *that* too, as a `declined` line in the selection table.
#[cfg(feature = "experimental-codecs")]
fn f64_codecs() -> Vec<(&'static str, Box<dyn Fn(&[f64]) -> Option<Vec<u8>>>)> {
	vec![("xor", Box::new(|values: &[f64]| Some(weft_physical_type::xor_f64_encode(values)))), ("chimp", Box::new(|values: &[f64]| Some(weft_physical_type::chimp_f64_encode(values)))), ("chimp128", Box::new(|values: &[f64]| Some(weft_physical_type::chimp128_f64_encode(values)))), ("elf", Box::new(weft_physical_type::elf_f64_encode))]
}

#[cfg(feature = "experimental-codecs")]
#[test]
fn f64_stream_goldens_pin_every_codec_on_every_corpus() {
	use weft_physical_type::{chimp128_f64_decode, chimp_f64_decode, elf_f64_decode, xor_f64_decode};
	for (corpus, values) in f64_corpora() {
		for (codec, encode) in f64_codecs() {
			// A codec that declines a corpus has no stream to pin, but the refusal is still a decision:
			// it is recorded as `declined` in the selection table below, so a codec that starts
			// accepting an input it used to refuse still fails this suite somewhere.
			let Some(bytes) = encode(&values) else { continue };
			check_golden(&format!("enc_f64_{codec}_{corpus}.bin"), &bytes);
			let decoded = match codec {
				"xor" => xor_f64_decode(&bytes, values.len()),
				"chimp" => chimp_f64_decode(&bytes, values.len()),
				"chimp128" => chimp128_f64_decode(&bytes, values.len()),
				"elf" => elf_f64_decode(&bytes, values.len()),
				other => unreachable!("{other} is not a codec this suite knows"),
			};
			assert_eq!(decoded, values, "{codec}/{corpus}: the codec does not round-trip its own golden corpus");
		}
	}
}

#[cfg(feature = "experimental-codecs")]
#[test]
fn f64_selection_golden_picks_the_same_codec_it_picked_today() {
	use weft_physical_type::{best_f64_bytes, best_f64_codec};
	let mut fresh = String::new();
	for (corpus, values) in f64_corpora() {
		let (winner, bytes) = best_f64_codec(&values);
		// The selector's own byte estimate has to agree with the codec it named, or a caller that
		// sized a buffer from `best_f64_bytes` and a caller that encoded would disagree about size.
		assert_eq!(bytes, best_f64_bytes(&values), "{corpus}: best_f64_codec and best_f64_bytes disagree");
		// And every codec's realised size, so a corpus that stops favouring its codec shows up here
		// with the numbers attached rather than as a mystery in a benchmark run.
		let mut sizes = Vec::new();
		for (codec, encode) in f64_codecs() {
			match encode(&values) {
				Some(bytes) => sizes.push(format!("{codec}={}", bytes.len())),
				None => sizes.push(format!("{codec}=declined")),
			}
		}
		writeln!(fresh, "{corpus}\twinner={winner}\tbest={bytes}\t{}", sizes.join("\t")).expect("writing to a String cannot fail");
	}
	check_golden_text("enc_f64_selection.txt", &fresh);
}

// ---------------------------------------------------------------------------------------------
// Directory hygiene, mirrored from `golden_frames.rs`.
// ---------------------------------------------------------------------------------------------

#[test]
fn no_orphan_encoding_goldens() {
	if updating() {
		// A recording run has not necessarily written every file yet when this test is scheduled.
		return;
	}
	let dir = harness::golden_dir();
	let entries: Vec<String> = std::fs::read_dir(&dir).expect("tests/golden must exist and be committed").filter_map(|entry| entry.ok().map(|entry| entry.file_name().to_string_lossy().into_owned())).filter(|name| name.starts_with("enc_")).collect();
	// Recompute the expected set from the same tables the tests write from, so this cannot drift from
	// them the way a hand-maintained list would.
	let mut expected: Vec<String> = Vec::new();
	for (corpus, values) in corpora() {
		let (width, payload) = bitpack_encode(&values);
		let _ = (width, payload);
		expected.push(format!("enc_ts_bitpack_{corpus}.bin"));
	}
	expected.push("enc_ts_rle.txt".to_string());
	expected.push("enc_ts_delta.txt".to_string());
	expected.push("enc_ts_zigzag.txt".to_string());
	#[cfg(feature = "experimental-codecs")]
	{
		for (corpus, values) in f64_corpora() {
			for (codec, encode) in f64_codecs() {
				if encode(&values).is_some() {
					expected.push(format!("enc_f64_{codec}_{corpus}.bin"));
				}
			}
		}
		expected.push("enc_f64_selection.txt".to_string());
	}
	for name in &entries {
		// The gated half of this suite owns the `enc_f64_` fixtures. In a build without the gate they
		// are on disk but unwritable, exactly as in `golden_frames.rs`: their presence is the gate
		// working, not an orphan.
		if name.starts_with("enc_f64_") && !cfg!(feature = "experimental-codecs") {
			continue;
		}
		assert!(expected.iter().any(|want| want == name), "{} contains {name}, which no codec case writes; delete it or add its case", dir.display());
	}
	for want in &expected {
		assert!(entries.iter().any(|have| have == want), "the codec suite writes {want}, but it is not committed in {}; record it with `WEFT_UPDATE_GOLDENS=1 cargo test -p weft-physical-type --all-features`", dir.display());
	}
}
