//! Schema-level per-aspect encoding **declaration** (roadmap **Phase 4.1 / 4.3**).
//!
//! [`crate::column::recommend_encoding`] is *advisory*: it surveys the data and
//! suggests the narrowest hot-path encoding that fits a tolerance. But the roadmap
//! is explicit that the encoding an aspect executes in is **schema-declared**, and
//! that any precision loss is "explicit, benchmarked, and governed by schema-level
//! error bounds — never a silent `BigDecimal → f64`" (hard constraint #4). This
//! module is that declaration.
//!
//! An [`AspectSchema`] fixes, per aspect, the [`PhysicalType`] its values are
//! stored in, the maximum per-value reconstruction error that physical encoding is
//! *allowed* to introduce, and the [`TimeUnit`] its timestamps are held in. Sealing
//! a batch of `(timestamp, value)` columns through the schema
//! ([`AspectSchema::seal`]) produces a [`Segment`] under the **declared** encoding —
//! and refuses, with a typed error, when the data cannot be represented or when the
//! declared encoding would exceed the declared error bound. That refusal is the
//! whole point: a schema-declared downcast that would silently lose more precision
//! than allowed is a hard error, not a quiet truncation.
//!
//! ## Declaration vs. recommendation
//!
//! | | [`Segment::build`] | [`AspectSchema::seal`] |
//! |---|---|---|
//! | encoding | chosen by `recommend_encoding` | **declared** by the schema |
//! | tolerance | guides the choice (always satisfiable — text backstops) | a **hard bound** the declared encoding must meet |
//! | over-tolerance | widens the encoding | **errors** ([`SealError::ToleranceExceeded`]) |
//! | unrepresentable value | cannot happen (text backstops) | **errors** ([`SealError::Encode`]) |
//!
//! Both produce the same [`Segment`] shape with identical statistics, so a sealed
//! segment is interchangeable downstream regardless of which path built it.

use bigdecimal::BigDecimal;

use crate::{
	column::{encode_column, ColumnEncodeError}, segment::SegmentStats, timestamp::{encode_delta_of_delta, TimeUnit}, PhysicalType, Segment, SEGMENT_FORMAT_VERSION
};

/// A per-aspect declaration of how that aspect's values and timestamps are stored.
///
/// This is the schema-level contract Storage v2 and the benchmark harness both read
/// to know an aspect's physical encoding without re-deriving it from the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AspectSchema {
	/// The declared physical encoding every value in the aspect is stored under.
	pub value: PhysicalType,
	/// The maximum per-value absolute reconstruction error the declared encoding is
	/// permitted to introduce. `0` demands a fully lossless encoding; a positive
	/// bound permits a declared, benchmarked downcast up to that error. Sealing a
	/// batch whose worst error exceeds this fails with
	/// [`SealError::ToleranceExceeded`].
	pub value_tolerance: BigDecimal,
	/// The epoch resolution the aspect's timestamps are held in.
	pub timestamp_unit: TimeUnit,
}

/// Why a batch could not be sealed into a [`Segment`] under an [`AspectSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealError {
	/// The timestamp and value columns had different lengths.
	LengthMismatch {
		/// Number of timestamps supplied.
		timestamps: usize,
		/// Number of values supplied.
		values: usize,
	},
	/// A value could not be represented at all under the declared encoding (overflow
	/// of a `ScaledI*` mantissa or a non-finite float) — carries the offending index.
	Encode(ColumnEncodeError),
	/// The declared encoding represented every value, but the worst per-value error
	/// exceeded the schema's [`value_tolerance`](AspectSchema::value_tolerance) — a
	/// declared downcast that loses more precision than allowed.
	ToleranceExceeded {
		/// The largest per-value absolute error the encoding actually produced.
		max_abs_error: BigDecimal,
		/// The bound the schema declared.
		tolerance: BigDecimal,
	},
}

impl std::fmt::Display for SealError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::LengthMismatch { timestamps, values } => write!(f, "schema seal column height mismatch: {timestamps} timestamps vs {values} values"),
			Self::Encode(e) => write!(f, "schema seal: {e}"),
			Self::ToleranceExceeded { max_abs_error, tolerance } => write!(f, "declared encoding exceeds tolerance: worst error {max_abs_error} > bound {tolerance}"),
		}
	}
}

impl std::error::Error for SealError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Encode(e) => Some(e),
			_ => None,
		}
	}
}

impl AspectSchema {
	/// Declare an aspect's storage: the physical value encoding, the per-value error
	/// bound it must honour, and the timestamp resolution.
	#[must_use]
	pub const fn new(value: PhysicalType, value_tolerance: BigDecimal, timestamp_unit: TimeUnit) -> Self {
		Self { value, value_tolerance, timestamp_unit }
	}

	/// Seal a batch of parallel `(timestamp, value)` columns into a [`Segment`] under
	/// this schema's **declared** encoding.
	///
	/// Unlike [`Segment::build`] — which picks the encoding advisorily and always
	/// succeeds — this enforces the declaration: the values are encoded under exactly
	/// [`self.value`](AspectSchema::value), and the result is rejected if any value is
	/// unrepresentable or if the worst reconstruction error exceeds
	/// [`self.value_tolerance`](AspectSchema::value_tolerance).
	///
	/// # Errors
	///
	/// - [`SealError::LengthMismatch`] if the columns differ in height.
	/// - [`SealError::Encode`] if a value cannot be represented under the declared
	///   encoding at all.
	/// - [`SealError::ToleranceExceeded`] if the declared encoding represents every
	///   value but loses more precision than the schema permits.
	pub fn seal(&self, timestamps: &[i64], values: &[BigDecimal]) -> Result<Segment, SealError> {
		if timestamps.len() != values.len() {
			return Err(SealError::LengthMismatch { timestamps: timestamps.len(), values: values.len() });
		}
		let value_col = encode_column(self.value, values).map_err(SealError::Encode)?;
		if value_col.max_abs_error > self.value_tolerance {
			return Err(SealError::ToleranceExceeded { max_abs_error: value_col.max_abs_error, tolerance: self.value_tolerance.clone() });
		}
		let ts_col = encode_delta_of_delta(timestamps, self.timestamp_unit);
		let stats = SegmentStats::from_columns(timestamps, values);
		Ok(Segment { version: SEGMENT_FORMAT_VERSION, values: value_col, timestamps: ts_col, stats })
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use super::*;

	fn col(lits: &[&str]) -> Vec<BigDecimal> {
		lits.iter().map(|s| BigDecimal::from_str(s).expect("parses")).collect()
	}

	#[test]
	fn seals_under_the_declared_encoding() {
		// Declared ScaledI64 at scale 2, lossless for 2-decimal data.
		let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, BigDecimal::from(0), TimeUnit::Seconds);
		let ts: Vec<i64> = (0..4).map(|i| 100 + i * 10).collect();
		let vs = col(&["1.25", "2.50", "-3.75", "0.00"]);
		let seg = schema.seal(&ts, &vs).expect("seals");
		// The segment is stored under the *declared* type, not an advisory pick.
		assert_eq!(seg.physical_type(), PhysicalType::ScaledI64 { scale: 2 });
		assert!(seg.is_exact());
		assert_eq!(seg.time_unit(), TimeUnit::Seconds);
		assert_eq!(seg.decode(), (ts, vs));
	}

	#[test]
	fn declared_encoding_differs_from_the_advisory_pick() {
		// recommend_encoding would take cheap F32 for these small integers; a schema
		// can *declare* the wider, exact ScaledI64 instead — and seal honours it.
		let vs = col(&["1", "2", "3"]);
		let advisory = crate::recommend_encoding(&vs, &BigDecimal::from(0));
		assert_eq!(advisory.physical_type, PhysicalType::F32);
		let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: 0 }, BigDecimal::from(0), TimeUnit::Millis);
		let seg = schema.seal(&[1, 2, 3], &vs).expect("seals");
		assert_eq!(seg.physical_type(), PhysicalType::ScaledI64 { scale: 0 });
	}

	#[test]
	fn lossless_declaration_rejects_a_lossy_batch() {
		// Declared F64 with a zero tolerance: 0.1 is not binary-exact, so the seal is
		// refused rather than silently downcast.
		let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
		let err = schema.seal(&[0, 1, 2], &col(&["0.5", "0.1", "0.3"])).expect_err("rejects lossy");
		match err {
			SealError::ToleranceExceeded { max_abs_error, tolerance } => {
				let zero = BigDecimal::from(0);
				assert!(max_abs_error > zero);
				assert_eq!(tolerance, zero);
			}
			other => panic!("expected ToleranceExceeded, got {other:?}"),
		}
	}

	#[test]
	fn generous_tolerance_admits_a_declared_downcast() {
		// The same lossy F64 batch seals when the schema permits the small error.
		let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from_str("0.001").unwrap(), TimeUnit::Seconds);
		let seg = schema.seal(&[0, 1, 2], &col(&["0.5", "0.1", "0.3"])).expect("seals within tolerance");
		assert_eq!(seg.physical_type(), PhysicalType::F64);
		assert!(!seg.is_exact(), "the declared downcast is honestly reported as lossy");
	}

	#[test]
	fn unrepresentable_value_is_a_typed_encode_error() {
		// 10^30 overflows a ScaledI64 mantissa: a declared encoding cannot fall back,
		// so the seal fails with the offending index.
		let mut huge = String::from("1");
		huge.push_str(&"0".repeat(30));
		let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: 0 }, BigDecimal::from(0), TimeUnit::Seconds);
		let err = schema.seal(&[0, 1], &col(&["1.0", &huge])).expect_err("overflows");
		match err {
			SealError::Encode(e) => assert_eq!(e.index, 1),
			other => panic!("expected Encode, got {other:?}"),
		}
	}

	#[test]
	fn length_mismatch_is_rejected() {
		let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
		let err = schema.seal(&[1, 2, 3], &col(&["1.0", "2.0"])).expect_err("mismatch");
		assert_eq!(err, SealError::LengthMismatch { timestamps: 3, values: 2 });
	}

	#[test]
	fn sealed_segment_round_trips_through_the_dspseg_frame() {
		// A schema-sealed segment is an ordinary Segment: it serialises and reloads.
		let schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: 3 }, BigDecimal::from(0), TimeUnit::Micros);
		let ts: Vec<i64> = (0..50).map(|i| 1_000 + i * 5).collect();
		let vs: Vec<BigDecimal> = (0..50).map(|i| BigDecimal::from_str(&format!("{i}.{:03}", (i * 13) % 1000)).unwrap()).collect();
		let seg = schema.seal(&ts, &vs).expect("seals");
		let back = Segment::read_from(&seg.write_to()).expect("reads");
		assert_eq!(back, seg);
		assert_eq!(back.decode(), (ts, vs));
	}

	#[test]
	fn empty_batch_seals_to_an_empty_segment() {
		let schema = AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds);
		let seg = schema.seal(&[], &[]).expect("seals");
		assert!(seg.is_empty());
		assert_eq!(seg.row_count(), 0);
	}
}
