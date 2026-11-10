//! ROB-9: the Parquet half of the mutation harness.
//!
//! `POST /api/v1/storage/{aspect}/ingest.parquet` hands a caller's bytes to
//! [`decode_parquet_columns_strict`]. A Parquet file has no checksum WeftDB can insist on, so
//! every byte of it is attacker-controlled: each byte substitution, each truncation and a seeded
//! corpus of multi-byte mutants of a real WeftDB-written file must decode or be refused with an
//! error, and **never panic** — a panic on the ingest path is a request the server answers with
//! a contained `500` at best, and a decoder that panics on one byte is a decoder that can be made
//! to.

use std::{
	panic::{catch_unwind, AssertUnwindSafe}, str::FromStr
};

use bigdecimal::BigDecimal;
use weft_arrow_store::decode_parquet_columns_strict;
use weft_physical_type::{PhysicalType, TimeUnit};

/// One named byte substitution.
type Substitution = (&'static str, fn(u8) -> u8);

const SUBSTITUTIONS: [Substitution; 5] = [("xor 0x01", |b| b ^ 0x01), ("xor 0x80", |b| b ^ 0x80), ("set 0x00", |_| 0x00), ("set 0x7f", |_| 0x7f), ("set 0xff", |_| 0xff)];

/// Random multi-byte mutants.
const RANDOM_MUTANTS: u64 = 2_048;

/// A small but real file: two row groups' worth of a typed decimal column with nulls, as the
/// WeftDB writer produces it.
fn golden() -> Vec<u8> {
	let timestamps: Vec<i64> = (0..64).map(|i| 1_700_000_000 + i * 60).collect();
	let values: Vec<Option<BigDecimal>> = (0..64).map(|i| (i % 7 != 3).then(|| BigDecimal::from_str(&format!("{}.25", i * 3)).expect("a decimal"))).collect();
	let batch = weft_arrow::columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, &timestamps, &values).expect("builds a batch");
	weft_arrow::write_parquet(&[batch]).expect("writes parquet")
}

/// Decode both ways; a panic comes back as its message.
fn decode(bytes: &[u8]) -> Result<(), String> {
	catch_unwind(AssertUnwindSafe(|| {
		let _ = decode_parquet_columns_strict(bytes, false);
		let _ = decode_parquet_columns_strict(bytes, true);
	}))
	.map_err(|panic| panic.downcast_ref::<&str>().map(ToString::to_string).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "a non-string panic".to_string()))
}

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
		usize::try_from(self.next() % u64::try_from(bound).expect("fits")).expect("below a usize")
	}
}

#[test]
fn every_mutant_of_a_parquet_file_is_decoded_or_refused_without_a_panic() {
	let golden = golden();
	assert!(decode_parquet_columns_strict(&golden, true).is_ok(), "the unmutated file decodes");
	let previous = std::panic::take_hook();
	std::panic::set_hook(Box::new(|_| {}));
	let mut failures = Vec::new();
	let mut mutants = 0usize;
	for offset in 0..golden.len() {
		for (label, substitute) in SUBSTITUTIONS {
			let mut mutant = golden.clone();
			if substitute(mutant[offset]) == mutant[offset] {
				continue;
			}
			mutant[offset] = substitute(mutant[offset]);
			mutants += 1;
			if let Err(panic) = decode(&mutant) {
				failures.push(format!("offset {offset} {label}: {panic}"));
			}
		}
	}
	for len in 0..golden.len() {
		mutants += 1;
		if let Err(panic) = decode(&golden[..len]) {
			failures.push(format!("truncated to {len}: {panic}"));
		}
	}
	for seed in 0..RANDOM_MUTANTS {
		let mut rng = SplitMix(seed);
		let mut mutant = golden.clone();
		for _ in 0..=rng.below(8) {
			let offset = rng.below(golden.len());
			mutant[offset] = u8::try_from(rng.next() & 0xff).expect("a byte");
		}
		mutants += 1;
		if let Err(panic) = decode(&mutant) {
			failures.push(format!("seed {seed}: {panic}"));
		}
	}
	std::panic::set_hook(previous);
	assert!(failures.is_empty(), "{} of {mutants} Parquet mutants panicked the ingest decoder; first: {:?}", failures.len(), &failures[..failures.len().min(20)]);
}
