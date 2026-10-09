//! Write-once frame names (docs/design/crash-consistency.md section 4; slice S8).
//!
//! A frame written by the write-once protocols is named
//! `{enc(aspect)}~g{gen}~p{prec}.weftseg`:
//!
//! - `enc` is [`aspect_name::encode`]: `[a-z0-9_]` literal, every other byte `%XX` in
//!   uppercase hex. It holds no `/`, `.`, `-` or `~`, so the name is a single plain path
//!   component, and it stays injective under case folding and Unicode normalisation;
//! - `gen` is the aspect's generation, from a per-aspect counter that never hands a value
//!   out twice, so no name is ever reused: a writer creates its frame with `create_new`,
//!   and nothing ever truncates or replaces a frame another writer could still read;
//! - `prec` is the adoption-order key: `gen` for a seal, the largest member `prec` for a
//!   maintenance output (a legacy member counts as 0).
//!
//! Legacy frames keep their `{aspect}-{id}.weftseg` names. Those always end in
//! `-{digits}.weftseg` and these never contain `-`, so the two forms are disjoint.
//!
//! **Long names.** `enc` can triple a name's length, and the aspect-name validator accepts
//! up to 160 bytes, so a name full of uppercase letters, punctuation or non-ASCII
//! characters could encode past the 255-byte file-name limit of the common filesystems.
//! When the encoded aspect would be longer than [`MAX_PLAIN_STEM_BYTES`], the stem is a
//! prefix of it (cut on an escape boundary) followed by `~h` and the 64-bit FNV-1a hash
//! of the full name in lowercase hex. Such a stem still contains no `-` and cannot be
//! mistaken for a plain one (a plain stem has no `~`); it no longer names its aspect by
//! itself, so [`FrameName::aspect`] is `None` for it and a caller that knows the aspect
//! compares stems ([`stem`]).

use crate::aspect_name;

/// The extension of every frame file.
const EXTENSION: &str = ".weftseg";

/// The longest encoded aspect a stem holds as it is. With the longest generation and
/// precedence (20 digits each) the file name is then at most 252 bytes.
pub const MAX_PLAIN_STEM_BYTES: usize = 200;

/// How much of an over-long encoded aspect a hashed stem keeps, at most.
const HASHED_PREFIX_BYTES: usize = 160;

/// The stem every write-once frame of `aspect` starts with: its encoded name, or for a
/// name that encodes past [`MAX_PLAIN_STEM_BYTES`], a prefix of that followed by `~h`
/// and the name's hash (see the module documentation).
#[must_use]
pub fn stem(aspect: &str) -> String {
	let encoded = aspect_name::encode(aspect);
	if encoded.len() <= MAX_PLAIN_STEM_BYTES {
		return encoded;
	}
	// Cut on an escape boundary: a `%` is never a literal, so a cut that would end inside
	// `%XX` moves back to its `%`.
	let bytes = encoded.as_bytes();
	let mut cut = HASHED_PREFIX_BYTES;
	if bytes[cut - 1] == b'%' {
		cut -= 1;
	} else if bytes[cut - 2] == b'%' {
		cut -= 2;
	}
	format!("{}~h{:016x}", &encoded[..cut], fnv1a64(aspect.as_bytes()))
}

/// The file name of `aspect`'s write-once frame of generation `gen` and adoption order
/// `prec`: `{stem}~g{gen}~p{prec}.weftseg` (see [`stem`]).
#[must_use]
pub fn frame_name(aspect: &str, gen: u64, prec: u64) -> String {
	format!("{}~g{gen}~p{prec}{EXTENSION}", stem(aspect))
}

/// A parsed write-once frame name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameName {
	/// The aspect's [`stem`].
	pub stem: String,
	/// The frame's generation.
	pub gen: u64,
	/// The frame's adoption-order key.
	pub prec: u64,
}

impl FrameName {
	/// The aspect the frame belongs to, when its stem names it (`None` for a hashed
	/// stem; compare [`stem`] of a known aspect instead).
	#[must_use]
	#[cfg_attr(not(test), expect(dead_code, reason = "recovery (S12) classifies the frames it lists by aspect; until then only the tests do"))]
	pub fn aspect(&self) -> Option<String> {
		aspect_name::decode(&self.stem)
	}
}

/// Parse `name` as a write-once frame name: exactly what [`frame_name`] writes, with a
/// canonical stem and canonical decimal fields, so `frame_name` of the parts gives `name`
/// back. `None` for anything else, including every legacy `{aspect}-{id}.weftseg` name.
#[must_use]
pub fn parse(name: &str) -> Option<FrameName> {
	let body = name.strip_suffix(EXTENSION)?;
	let (rest, prec) = body.rsplit_once("~p")?;
	let (stem, gen) = rest.rsplit_once("~g")?;
	let (gen, prec) = (canonical_u64(gen)?, canonical_u64(prec)?);
	canonical_stem(stem).then(|| FrameName { stem: stem.to_string(), gen, prec })
}

/// Whether `stem` is a stem [`stem`] can write: a non-empty canonical encoded name of at
/// most [`MAX_PLAIN_STEM_BYTES`], or a hashed one.
fn canonical_stem(stem: &str) -> bool {
	match stem.split_once("~h") {
		None => !stem.is_empty() && stem.len() <= MAX_PLAIN_STEM_BYTES && aspect_name::decode(stem).is_some(),
		Some((prefix, hash)) => {
			let escapes_whole = prefix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'%' || (b'A'..=b'F').contains(&b));
			!prefix.is_empty() && prefix.len() <= HASHED_PREFIX_BYTES && escapes_whole && hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
		}
	}
}

/// `digits` as a `u64` when it is written exactly as `{}` writes one: ASCII digits, no
/// sign, no leading zero (except `0` itself).
fn canonical_u64(digits: &str) -> Option<u64> {
	if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || (digits.len() > 1 && digits.starts_with('0')) {
		return None;
	}
	digits.parse().ok()
}

/// The 64-bit FNV-1a hash of `bytes`: stable across builds and platforms, unlike std's
/// `DefaultHasher`, because it names files.
fn fnv1a64(bytes: &[u8]) -> u64 {
	bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_frame_name_is_the_encoded_aspect_the_generation_and_the_precedence() {
		assert_eq!(frame_name("price", 3, 0), "price~g3~p0.weftseg");
		assert_eq!(frame_name("Room-A.temp", 12, 7), "%52oom%2D%41%2Etemp~g12~p7.weftseg");
		assert_eq!(frame_name("job:rate", u64::MAX, u64::MAX), format!("job%3Arate~g{}~p{}.weftseg", u64::MAX, u64::MAX));
	}

	#[test]
	fn names_parse_back_to_their_parts() {
		for (aspect, gen, prec) in [("price", 1, 0), ("Room-A.temp", 12, 7), ("温度", 0, 0), ("a~g1~p2", 5, 5), ("x-1", u64::MAX, 3)] {
			let name = frame_name(aspect, gen, prec);
			let parsed = parse(&name).unwrap_or_else(|| panic!("{name} parses"));
			assert_eq!((parsed.aspect().as_deref(), parsed.gen, parsed.prec), (Some(aspect), gen, prec), "{name}");
			assert_eq!(parsed.stem, stem(aspect));
		}
	}

	/// Legacy names, litter and non-canonical spellings are not write-once frame names, so
	/// recovery and the generation seed never mistake one for another.
	#[test]
	fn only_canonical_write_once_names_parse() {
		for other in ["price-0.weftseg", "price-12.weftpart", "price~g1~p0.weftpart", "price~g01~p0.weftseg", "price~g1~p00.weftseg", "price~g~p0.weftseg", "price~g1.weftseg", "Price~g1~p0.weftseg", "pr-ice~g1~p0.weftseg", "%61~g1~p0.weftseg", "price~g+1~p0.weftseg", "price~g1~p0.weftseg.tmp", "~g1~p0.weftseg", "LOCK"] {
			assert_eq!(parse(other), None, "{other}");
		}
	}

	/// A name that would encode past the file-name limit gets a bounded, hashed stem that
	/// still parses, still holds no `-`, and differs for names that share a long prefix.
	#[test]
	fn an_over_long_encoded_aspect_gets_a_bounded_hashed_stem() {
		let long = "É".repeat(80); // 160 bytes, each escaped: 480 encoded
		let other = format!("{}e", "É".repeat(79));
		let (a, b) = (stem(&long), stem(&other));
		assert!(a.contains("~h") && b.contains("~h"), "{a} {b}");
		assert_ne!(a, b, "names sharing a long prefix keep distinct stems");
		let name = frame_name(&long, u64::MAX, u64::MAX);
		assert!(name.len() <= 255, "{} bytes", name.len());
		assert!(!name.contains('-'), "never mistaken for a legacy name");
		let parsed = parse(&name).expect("parses");
		assert_eq!((parsed.stem.as_str(), parsed.aspect()), (a.as_str(), None));
		// Cut on an escape boundary whatever the alignment: every `%` the prefix holds is
		// followed by its two digits.
		for pad in 0..3 {
			let name = format!("{}{}", "a".repeat(pad), "É".repeat(78));
			let stem = stem(&name);
			let prefix = stem.split_once("~h").expect("hashed").0.as_bytes();
			let whole = prefix.iter().enumerate().filter(|(_, b)| **b == b'%').all(|(at, _)| prefix.get(at + 2).is_some_and(u8::is_ascii_hexdigit) && prefix[at + 1].is_ascii_hexdigit());
			assert!(whole && prefix.len() <= HASHED_PREFIX_BYTES, "{} ends on an escape boundary", String::from_utf8_lossy(prefix));
		}
		// The longest plain stem stays plain.
		let plain = "a".repeat(MAX_PLAIN_STEM_BYTES);
		assert_eq!(stem(&plain), plain);
	}

	#[test]
	fn the_hash_is_fnv1a() {
		assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
		assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
	}
}
