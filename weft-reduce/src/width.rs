//! **Bucket widths** that are a whole number of resolution steps: `5s`, `15m`, `6h`.
//!
//! splimes' [`Resolution`] names one step of each unit, so a reduction keyed by it can bucket
//! by the second or the minute but not by five seconds — the TSM-Bench interpolation query's
//! literal `SAMPLE BY 5s` (Q5) had no spelling. A [`BucketWidth`] is a unit and a count. Its
//! buckets are aligned to the epoch like the unit's own (bucket `i` starts at `i · count`
//! unit steps), so a width of one step is exactly its [`Resolution`], and every surface keyed
//! by a resolution keeps its meaning.
//!
//! A reduction at a width is the reduction at its unit with each run of `count` unit buckets
//! merged into one ([`PartialReduction::coarsen`](crate::PartialReduction::coarsen)), which
//! is exact for every aggregation, the percentiles and time-weighted averages included.

use std::fmt;

use chrono::{DateTime, Utc};
use splimes::Resolution;

use crate::{bucket_index, bucket_start, ReduceError};

/// A bucket width: `count` steps of `unit`, aligned to the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BucketWidth {
	unit: Resolution,
	count: u32,
}

impl From<Resolution> for BucketWidth {
	fn from(unit: Resolution) -> Self {
		Self { unit, count: 1 }
	}
}

impl BucketWidth {
	/// `count` steps of `unit`; `None` for a count of zero.
	#[must_use]
	pub const fn new(unit: Resolution, count: u32) -> Option<Self> {
		if count == 0 {
			None
		} else {
			Some(Self { unit, count })
		}
	}

	/// The unit a bucket is a whole number of.
	#[must_use]
	pub const fn unit(self) -> Resolution {
		self.unit
	}

	/// How many unit steps one bucket spans.
	#[must_use]
	pub const fn count(self) -> u32 {
		self.count
	}

	/// The width in nanoseconds.
	#[must_use]
	pub const fn step_nanos(self) -> i128 {
		self.unit.step_nanos() as i128 * self.count as i128
	}

	/// Parse a width: a unit name (`minutes`, as [`Resolution`] spells it), or a count and a
	/// unit with no space between (`5s`, `15m`, `250ms`, `6h`, `2d`, `1w`) or one space
	/// (`5 seconds`, `15 minutes`). Units: `ns`, `us`, `ms`, `s`, `m`, `h`, `d`, `w`, and
	/// [`Resolution`]'s names in singular or plural.
	#[must_use]
	pub fn parse(token: &str) -> Option<Self> {
		let token = token.trim();
		let digits = token.find(|c: char| !c.is_ascii_digit()).unwrap_or(token.len());
		let (count, unit) = if digits == 0 { (1, token) } else { (token[..digits].parse().ok()?, token[digits..].trim_start()) };
		let unit = match unit.to_ascii_lowercase().as_str() {
			"ns" => Resolution::Nanoseconds,
			"us" | "µs" => Resolution::Microseconds,
			"ms" => Resolution::Milliseconds,
			"s" => Resolution::Seconds,
			"m" => Resolution::Minutes,
			"h" => Resolution::Hours,
			"d" => Resolution::Days,
			"w" => Resolution::Weeks,
			name => name.parse::<Resolution>().ok().or_else(|| format!("{name}s").parse().ok())?,
		};
		Self::new(unit, count)
	}

	/// The index of the bucket holding `timestamp`.
	///
	/// # Errors
	///
	/// [`ReduceError::TimestampRange`] if the timestamp cannot be indexed at the unit.
	pub fn index(self, timestamp: &DateTime<Utc>) -> Result<i64, ReduceError> {
		Ok(bucket_index(self.unit, timestamp)?.div_euclid(i64::from(self.count)))
	}

	/// The start of bucket `index`; `None` if it lies outside the representable range.
	#[must_use]
	pub fn start(self, index: i64) -> Option<DateTime<Utc>> {
		bucket_start(self.unit, index.checked_mul(i64::from(self.count))?)
	}
}

impl fmt::Display for BucketWidth {
	/// The unit's name for one step (`minutes`), otherwise the count and the name (`15 minutes`).
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.count == 1 {
			write!(f, "{}", self.unit)
		} else {
			write!(f, "{} {}", self.count, self.unit)
		}
	}
}

#[cfg(test)]
mod tests {
	use chrono::TimeZone;

	use super::*;

	#[test]
	fn widths_parse_in_short_long_and_bare_forms() {
		assert_eq!(BucketWidth::parse("5s"), BucketWidth::new(Resolution::Seconds, 5));
		assert_eq!(BucketWidth::parse("15m"), BucketWidth::new(Resolution::Minutes, 15));
		assert_eq!(BucketWidth::parse("250ms"), BucketWidth::new(Resolution::Milliseconds, 250));
		assert_eq!(BucketWidth::parse("15 minutes"), BucketWidth::new(Resolution::Minutes, 15));
		assert_eq!(BucketWidth::parse("1 hour"), BucketWidth::new(Resolution::Hours, 1));
		assert_eq!(BucketWidth::parse("minutes"), Some(BucketWidth::from(Resolution::Minutes)));
		for bad in ["0s", "5", "5 parsecs", "", "s5"] {
			assert_eq!(BucketWidth::parse(bad), None, "{bad:?}");
		}
	}

	#[test]
	fn buckets_are_epoch_aligned_multiples_of_the_unit() {
		let w = BucketWidth::new(Resolution::Seconds, 5).expect("width");
		let t = Utc.timestamp_opt(1_007, 0).single().expect("instant");
		assert_eq!(w.index(&t), Ok(201));
		assert_eq!(w.start(201), Utc.timestamp_opt(1_005, 0).single());
		let before = Utc.timestamp_opt(-1, 0).single().expect("instant");
		assert_eq!(w.index(&before), Ok(-1), "floored, not truncated, before the epoch");
		assert_eq!((w.to_string(), BucketWidth::from(Resolution::Minutes).to_string()), ("5 seconds".to_string(), "minutes".to_string()));
		assert_eq!(w.step_nanos(), 5_000_000_000);
	}
}
