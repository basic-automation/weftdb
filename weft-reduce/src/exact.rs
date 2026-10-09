//! Serializers for the [`BigDecimal`]s a [`PartialReduction`](crate::PartialReduction)
//! holds, keeping each one's scale.
//!
//! A `BigDecimal` is `digits × 10^-scale`. `bigdecimal` serializes one as its `Display`
//! string, which loses the scale of a zero (`0.000`, scale 3, is written `"0"`) and of a
//! decimal with a small negative scale (`5E+2`, scale -2, is written `"500"`). The value
//! reads back equal, but a reduction finished from the reloaded partial then reports a
//! zero `min` or `sum` at scale 0 where the same reduction over the decoded values reports
//! it at the column's scale, so exact-decimal output prints `0` for `0.000`. These write a
//! zero with a positive scale, and any decimal with a negative scale, in scientific form,
//! `{digits}e{-scale}` (`"0e-3"`, `"5e2"`), and every other decimal exactly as `bigdecimal`
//! does, so the encoding changes only for them. Reading needs nothing extra: `bigdecimal`
//! parses `"0e-3"` back to `0.000`, and still parses the `"0"` an older partial holds (at
//! scale 0).

use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};

/// A decimal that serializes with its scale.
struct Exact<'a>(&'a BigDecimal);

impl Serialize for Exact<'_> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		// `Display` keeps the scale of a non-zero decimal with a positive scale, and of any
		// decimal at scale 0.
		let (digits, scale) = self.0.as_bigint_and_scale();
		if scale < 0 || (scale > 0 && digits.is_zero()) {
			serializer.collect_str(&format_args!("{digits}e{}", -i128::from(scale)))
		} else {
			self.0.serialize(serializer)
		}
	}
}

/// A `BigDecimal` field.
pub fn decimal<S: Serializer>(value: &BigDecimal, serializer: S) -> Result<S::Ok, S::Error> {
	Exact(value).serialize(serializer)
}

/// An `Option<BigDecimal>` field.
#[allow(clippy::ref_option)] // `serialize_with` passes a reference to the field.
pub fn option<S: Serializer>(value: &Option<BigDecimal>, serializer: S) -> Result<S::Ok, S::Error> {
	value.as_ref().map(Exact).serialize(serializer)
}

/// An `Option<(DateTime<Utc>, BigDecimal)>` field.
#[allow(clippy::ref_option)] // `serialize_with` passes a reference to the field.
pub fn stamped<S: Serializer>(value: &Option<(DateTime<Utc>, BigDecimal)>, serializer: S) -> Result<S::Ok, S::Error> {
	value.as_ref().map(|(t, v)| (t, Exact(v))).serialize(serializer)
}

/// A `Vec<(DateTime<Utc>, BigDecimal)>` field.
pub fn samples<S: Serializer>(value: &[(DateTime<Utc>, BigDecimal)], serializer: S) -> Result<S::Ok, S::Error> {
	serializer.collect_seq(value.iter().map(|(t, v)| (t, Exact(v))))
}
