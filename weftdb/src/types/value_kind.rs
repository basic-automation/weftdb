//! **The provenance vocabulary for a value a read path hands back** — roadmap slice **TAG-3**.
//!
//! Every value WeftDB returns came from somewhere, and the three ways it can arrive are already
//! distinguishable inside the compute paths: a stored sample, a value synthesised *between* two
//! stored samples, or a value projected *past* the last one. splimes models exactly these as
//! [`PointKind`], and the compute endpoints put splimes' enum straight on the wire: `weft-server`
//! re-exports it (`weft-server/src/interpolate.rs`) as the response's `kind`, with the tokens `raw`,
//! `interpolated` and `extrapolated`.
//!
//! That is fine as long as the vocabulary never grows. It is going to grow. A value can also be
//! **reconstructed rather than sampled** — from a lossy codec that trades the last digits for bytes,
//! or from a stored partial reduction that answers a coarse query without decoding the segment — and
//! both are on the 1.0 plan (the bit-sliced codec already ships behind a feature; the `.weftpart`
//! sidecar already answers downsamples). A client that cannot tell a stored sample from a
//! reconstructed value cannot tell how much to trust it, so those kinds have to be visible in the
//! result rather than inferred from the request that produced it.
//!
//! **So this vocabulary is open, and splimes' is not.** [`PointKind`] is an exhaustive enum: adding a
//! variant is a breaking change for every caller that matches on it, which is precisely why WeftDB
//! must not put a closed third-party enum in front of its own clients — a client written today would
//! fail to compile against a server that learned one more kind tomorrow. [`ValueKind`] is
//! `#[non_exhaustive]` instead, and it does not even wait for a future release
//! to be open-ended: a token this build does not recognise parses to [`ValueKind::Unknown`] carrying
//! the token itself, so a *newer* vocabulary reading through an *older* client degrades to
//! "some kind I have not been told about" rather than a parse error or a wrong `match` arm. The
//! client rule falls out of the shape, and is the whole reason for it:
//!
//! ```
//! use weftdb::ValueKind;
//!
//! fn describe(kind: &ValueKind) -> String {
//! 	match kind {
//! 		ValueKind::Raw => "a stored sample".to_string(),
//! 		// Every other kind — this build's interpolated and extrapolated, any reserved token
//! 		// that arrives as `Unknown`, and any variant added in a later release — is handled
//! 		// by this arm, which is why the enum can stay open.
//! 		other => format!("a derived value ({})", other.as_str()),
//! 	}
//! }
//!
//! assert_eq!(describe(&ValueKind::Raw), "a stored sample");
//! assert_eq!(describe(&ValueKind::Interpolated), "a derived value (interpolated)");
//! // A token from a vocabulary newer than this crate still round-trips instead of failing.
//! let from_the_future = ValueKind::parse("reconstructed");
//! assert!(from_the_future.is_unknown(), "an unrecognised token is preserved, not rejected");
//! assert_eq!(describe(&from_the_future), "a derived value (reconstructed)");
//! ```
//!
//! ## The scope of the vocabulary
//!
//! `ValueKind` classifies **how a value in a result was produced**. It says nothing about *which*
//! segment, aspect or replica a value came from, and it is not a confidence score: `Interpolated`
//! says the value was synthesised by an [`Interpolator`](splimes::Interpolator), not that the
//! synthesis was a good guess. Two kinds are deliberately *not* modelled here because they are
//! properties of a series rather than of a value: whether timestamps were ordered on ingest (the
//! segment descriptor's `time_sorted`) and whether a read was served from a stale-accelerated sidecar
//! (which [`PartialSidecar::matches`](crate::PartialSidecar::matches) answers by *not* matching).
//!
//! **Wire tokens are the compatibility surface**, not the variant names: [`ValueKind::as_str`] and
//! the [`serde`] impls emit and accept exactly `raw`, `interpolated` and `extrapolated`, matching the
//! tokens the compute endpoints already send. The reserved tokens in
//! [`RESERVED_VALUE_KIND_TOKENS`] are promised to those names *before* they become variants, so a
//! client that special-cases them today keeps working when they become real variants — and a client
//! that does not must keep treating them as [`ValueKind::Unknown`], which is what parsing them yields
//! until then.

use std::{convert::Infallible, fmt};

use serde::{de::Deserializer, ser::Serializer};
use splimes::PointKind;

/// The tokens reserved for kinds that are planned but not yet produced by any read path.
///
/// Parsing one of these today gives [`ValueKind::Unknown`]; the name is held so that when the kind
/// becomes a variant the token does not change underneath a client that already learned it. A client
/// must not infer *which* future kind a reserved token is: the token is the whole contract.
pub const RESERVED_VALUE_KIND_TOKENS: &[&str] = &["compressed", "reconstructed"];

/// How the value at this instant was produced.
///
/// **This enum is `#[non_exhaustive]`: match it with a wildcard arm.** A later release adds kinds
/// (see [`RESERVED_VALUE_KIND_TOKENS`]) and that must not be a compile error for a caller; a token
/// this version cannot name arrives as [`ValueKind::Unknown`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ValueKind {
	/// The stored sample: the value as it was written, decoded exactly.
	Raw,
	/// Synthesised between stored samples by an interpolation.
	Interpolated,
	/// Projected beyond the first or last stored sample by an extrapolation.
	Extrapolated,
	/// A kind this build of WeftDB does not name, preserved as the token that carried it.
	///
	/// Produced by parsing or deserializing an unrecognised token — never by a WeftDB read path,
	/// which always knows the kind it is reporting.
	Unknown(UnknownValueKind),
}

impl ValueKind {
	/// The wire token for this kind. For [`ValueKind::Unknown`] this is the token that was parsed,
	/// byte for byte, so a kind this build cannot name still round-trips out again.
	#[must_use]
	pub fn as_str(&self) -> &str {
		match self {
			Self::Raw => "raw",
			Self::Interpolated => "interpolated",
			Self::Extrapolated => "extrapolated",
			Self::Unknown(unknown) => unknown.as_str(),
		}
	}

	/// Whether this build has a name for the kind.
	///
	/// `false` means only "this version does not name it" — not "the value is wrong" and not "the
	/// sender is broken": an older client reading a newer vocabulary is the expected case.
	#[must_use]
	pub const fn is_unknown(&self) -> bool {
		matches!(self, Self::Unknown(_))
	}

	/// The token a caller can hand back to a WeftDB endpoint, for `Unknown` as faithfully as for a
	/// named kind.
	#[must_use]
	pub fn parse(token: &str) -> Self {
		match token {
			"raw" => Self::Raw,
			"interpolated" => Self::Interpolated,
			"extrapolated" => Self::Extrapolated,
			other => Self::Unknown(UnknownValueKind { token: other.into() }),
		}
	}
}

/// A provenance token this build of WeftDB does not name.
///
/// Opaque on purpose: its only job is to carry the token through a client that cannot interpret it,
/// so a `match` on [`ValueKind`] cannot accidentally treat an unknown kind as a known one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnknownValueKind {
	token: Box<str>,
}

impl UnknownValueKind {
	/// The token exactly as it arrived.
	#[must_use]
	pub fn as_str(&self) -> &str {
		&self.token
	}
}

impl fmt::Display for UnknownValueKind {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.token)
	}
}

impl fmt::Display for ValueKind {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.as_str())
	}
}

impl std::str::FromStr for ValueKind {
	type Err = Infallible;

	fn from_str(token: &str) -> Result<Self, Self::Err> {
		Ok(Self::parse(token))
	}
}

impl From<PointKind> for ValueKind {
	/// The kinds splimes reports today are a subset of this vocabulary, and the mapping is total:
	/// splimes' enum is closed, so a read path can convert without a fallback arm.
	fn from(kind: PointKind) -> Self {
		match kind {
			PointKind::Raw => Self::Raw,
			PointKind::Interpolated => Self::Interpolated,
			PointKind::Extrapolated => Self::Extrapolated,
		}
	}
}

/// Back out to splimes' closed enum for a call that needs one. Fails for exactly the kinds splimes
/// cannot express — an unrecognised token, or a kind this version has not named yet.
impl TryFrom<ValueKind> for PointKind {
	type Error = UnknownValueKind;

	fn try_from(kind: ValueKind) -> Result<Self, Self::Error> {
		match kind {
			ValueKind::Raw => Ok(Self::Raw),
			ValueKind::Interpolated => Ok(Self::Interpolated),
			ValueKind::Extrapolated => Ok(Self::Extrapolated),
			ValueKind::Unknown(unknown) => Err(unknown),
		}
	}
}

impl serde::Serialize for ValueKind {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_str(self.as_str())
	}
}

impl<'de> serde::Deserialize<'de> for ValueKind {
	/// Never fails on an unrecognised token: an unknown kind is data, not a malformed document. A
	/// client that must reject unknown kinds can check [`ValueKind::is_unknown`] after parsing.
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		Ok(Self::parse(&String::deserialize(deserializer)?))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_wire_tokens_are_the_ones_the_compute_endpoints_already_send() {
		// These three strings are the compatibility surface: the compute endpoints put them in a JSON
		// `kind` field today (the token table is pinned there too), so changing one here is a wire
		// change and must be a deliberate one.
		assert_eq!(ValueKind::Raw.as_str(), "raw");
		assert_eq!(ValueKind::Interpolated.as_str(), "interpolated");
		assert_eq!(ValueKind::Extrapolated.as_str(), "extrapolated");
		for kind in [ValueKind::Raw, ValueKind::Interpolated, ValueKind::Extrapolated] {
			assert_eq!(ValueKind::parse(kind.as_str()), kind, "{kind} must round-trip through its token");
			assert_eq!(kind.to_string(), kind.as_str(), "Display is the token, not the variant name");
		}
	}

	#[test]
	fn an_unknown_token_is_preserved_rather_than_rejected() {
		let unknown = ValueKind::parse("reconstructed");
		assert!(unknown.is_unknown(), "a token this build does not name must not become a known kind");
		assert_eq!(ValueKind::parse("reconstructed").as_str(), "reconstructed", "the token survives verbatim");
		assert_eq!(serde_json::to_string(&unknown).expect("serializes"), "\"reconstructed\"", "and it goes back out as what came in");

		// A token that is merely *almost* a known one is unknown, not a typo-corrected match.
		assert!(ValueKind::parse("Raw").is_unknown(), "tokens are case-sensitive");
		assert!(ValueKind::parse("raw ").is_unknown(), "tokens are not trimmed");
		let empty = ValueKind::parse("");
		assert!(empty.is_unknown() && empty.as_str().is_empty(), "an empty token stays an empty token");
	}

	#[test]
	fn reserved_tokens_are_reserved_and_not_yet_named() {
		for token in RESERVED_VALUE_KIND_TOKENS {
			assert!(ValueKind::parse(token).is_unknown(), "{token} is reserved: no variant carries it yet");
			// The reservation is only meaningful if the token is what a future variant will emit.
			let json = serde_json::to_string(&ValueKind::parse(token)).expect("serializes");
			assert_eq!(json, format!("\"{token}\""), "a reserved token round-trips, so a client may special-case it now");
		}
	}

	#[test]
	fn serde_carries_the_token_not_the_variant() {
		let json = serde_json::to_string(&ValueKind::Extrapolated).expect("serializes");
		assert_eq!(json, "\"extrapolated\"");
		let kinds: Vec<ValueKind> = serde_json::from_str(r#"["raw","interpolated","whatever_comes_next"]"#).expect("a document with a future token parses");
		assert_eq!(kinds[0], ValueKind::Raw);
		assert_eq!(kinds[1], ValueKind::Interpolated);
		assert_eq!(kinds[2].as_str(), "whatever_comes_next");
		assert!(kinds[2].is_unknown());
	}

	#[test]
	fn splimes_kinds_map_both_ways() {
		for (point_kind, kind) in [(PointKind::Raw, ValueKind::Raw), (PointKind::Interpolated, ValueKind::Interpolated), (PointKind::Extrapolated, ValueKind::Extrapolated)] {
			let widened = ValueKind::from(point_kind);
			assert_eq!(widened, kind);
			assert_eq!(PointKind::try_from(widened.clone()).expect("splimes can express a kind it reported"), point_kind);
		}
		// The other direction is where the vocabulary is wider, and the refusal carries the token.
		let err = PointKind::try_from(ValueKind::parse("compressed")).expect_err("splimes has no such kind");
		assert_eq!(err.as_str(), "compressed");
	}

	#[test]
	fn parsing_never_fails() {
		// The `FromStr` infallibility is what lets a client parse a `kind` field without an error
		// path it would be tempted to treat as "absent".
		let parsed: ValueKind = "interpolated".parse().expect("infallible");
		assert_eq!(parsed, ValueKind::Interpolated);
		let fallback: ValueKind = "who_knows".parse().expect("still infallible");
		assert!(fallback.is_unknown());
	}
}
