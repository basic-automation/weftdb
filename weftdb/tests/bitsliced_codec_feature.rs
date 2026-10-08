//! `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` keeps working as a name in every build, but only a
//! build with the `bitsliced-codec` feature acts on it: without the feature the bit-sliced
//! value codec is not compiled in, so the variable is ignored (with a warning) rather than
//! half-honoured.
//!
//! Each build mode compiles exactly one test here, so this binary is the only code in its
//! process touching the variable — no cross-test race on the environment.

use weftdb::TransposedPolicy;

const VAR: &str = "WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD";

#[cfg(not(feature = "bitsliced-codec"))]
#[test]
fn the_env_var_is_ignored_without_the_feature() {
	std::env::set_var(VAR, "1.05");
	let policy = TransposedPolicy::from_env();
	std::env::remove_var(VAR);
	assert_eq!(policy, TransposedPolicy::DISABLED, "a build without the codec must never select it");
}

#[cfg(feature = "bitsliced-codec")]
#[test]
fn the_env_var_sets_the_ceiling_with_the_feature() {
	std::env::set_var(VAR, "1.05");
	let policy = TransposedPolicy::from_env();
	std::env::remove_var(VAR);
	assert_eq!(policy, TransposedPolicy { max_overhead: Some(1.05) });
}
