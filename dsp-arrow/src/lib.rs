//! # dsp-arrow
//!
//! Apache Arrow interchange for DSP's typed columnar segments (roadmap
//! **Phase 4.5**: *Arrow-compatible arrays — eases Python/Flight/DataFusion/
//! Parquet*; Phase 4.3 names *Arrow-compatible memory internally*).
//!
//! ## Why this is its own crate
//!
//! Arrow is the lingua franca a DSP segment must speak to reach the wider
//! ecosystem — Python (`pyarrow`), Arrow Flight / Flight SQL, `DataFusion`, and
//! Parquet all consume `RecordBatch`es. But the `arrow-*` crates are a large
//! dependency tree, and the workspace hard constraints keep the measurement
//! hot-path crates (`splimes`, `database`, `dsp-physical-type`) lean. So the
//! conversion lives **here**, in a leaf crate that depends on
//! [`dsp_physical_type`] (never the reverse): the heavy dependency never reaches
//! the core.
//!
//! ## Vendor-neutrality (hard constraints #2 and #3)
//!
//! Apache Arrow is an **open, vendor-neutral in-memory interchange standard**
//! governed by the Apache Software Foundation — not a vendor product and not a
//! storage backend. Converting a [`Segment`] to a [`RecordBatch`] is an
//! *interchange* operation: it does not make Arrow the measurement store
//! (constraint #3 — DSP's own `.dspseg` columnar segments still own the hot path),
//! and it introduces no vendor-specific connector into the core (constraint #2).
//! Per the roadmap, *Parquet import/export from day one* and *Arrow-compatible
//! memory internally* are explicit goals; this crate is the first slice of them.
//!
//! ## What this slice covers
//!
//! A **lossless** round trip between a sealed [`Segment`] and an Arrow
//! [`RecordBatch`]:
//!
//! - [`segment_to_record_batch`] emits a two-column batch — a non-null
//!   `timestamp: Int64` column (the integer epochs, exactly as the segment stores
//!   them) and a nullable `value: Utf8` column holding each present value's
//!   **plain decimal text** (`BigDecimal::to_plain_string`), with Arrow validity
//!   marking the null/absent rows. Decimal text is the always-exact encoding (the
//!   [`PhysicalType::BigDecimalText`](dsp_physical_type::PhysicalType::BigDecimalText)
//!   philosophy): no value can lose a digit crossing into Arrow, honoring hard
//!   constraint #4 (no silent downcast). The segment's [`TimeUnit`], physical
//!   encoding name, and format version travel in the schema metadata so the batch
//!   is self-describing.
//! - [`record_batch_to_columns`] reads that batch back into the logical
//!   `(Vec<i64>, Vec<Option<BigDecimal>>)` columns, and
//!   [`segment_from_record_batch`] re-seals them into a [`Segment`] under a
//!   caller-supplied value tolerance, recovering the [`TimeUnit`] from metadata.
//!
//! A typed numeric fast path (Arrow `Float64`/`Decimal128` arrays for the
//! corresponding [`PhysicalType`]s) and `PagedSegment` / Parquet export are the
//! next slices; this one establishes the lossless contract everything else must
//! preserve.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use std::{collections::HashMap, sync::Arc};

use arrow_array::{Array, ArrayRef, Float32Array, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use dsp_physical_type::{PhysicalType, Segment, SegmentError, TimeUnit};

/// Name of the timestamp column in an exported [`RecordBatch`].
pub const TIMESTAMP_COLUMN: &str = "timestamp";
/// Name of the value column in an exported [`RecordBatch`].
pub const VALUE_COLUMN: &str = "value";

/// Schema-metadata key carrying the segment's [`TimeUnit`] name.
pub const META_TIME_UNIT: &str = "dsp:time_unit";
/// Schema-metadata key carrying the segment's physical-encoding name (advisory —
/// this slice always serializes values as decimal text, recorded under
/// [`META_VALUE_ENCODING`]).
pub const META_PHYSICAL_TYPE: &str = "dsp:physical_type";
/// Schema-metadata key carrying the wire form of the value column.
pub const META_VALUE_ENCODING: &str = "dsp:value_encoding";
/// Schema-metadata key carrying the segment's format version.
pub const META_FORMAT_VERSION: &str = "dsp:format_version";

/// Value-column wire form: lossless plain decimal text (Arrow `Utf8`). The
/// fallback for any encoding without a natural fixed-width Arrow array, and the
/// always-exact form.
pub const VALUE_ENCODING_TEXT: &str = "text";
/// Value-column wire form: IEEE-754 binary64 (Arrow `Float64`) — the typed fast
/// path for an [`PhysicalType::F64`](dsp_physical_type::PhysicalType::F64) segment.
pub const VALUE_ENCODING_F64: &str = "f64";
/// Value-column wire form: IEEE-754 binary32 (Arrow `Float32`) — the typed fast
/// path for an [`PhysicalType::F32`](dsp_physical_type::PhysicalType::F32) segment.
pub const VALUE_ENCODING_F32: &str = "f32";

/// Why a [`RecordBatch`] could not be converted back into DSP columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
	/// The batch is missing a required column.
	MissingColumn(&'static str),
	/// A column had an Arrow type other than the one this slice produces.
	WrongType {
		/// The offending column's name.
		column: &'static str,
		/// The Arrow type expected for it.
		expected: &'static str,
	},
	/// Required schema metadata (e.g. the [`TimeUnit`]) was absent.
	MissingMetadata(&'static str),
	/// The recorded time-unit name did not match any [`TimeUnit`].
	UnknownTimeUnit(String),
	/// A value cell held text that does not parse as a `BigDecimal`.
	BadValue(String),
	/// The two columns disagreed on height, or re-sealing the segment failed.
	Segment(SegmentError),
}

impl std::fmt::Display for ConvertError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::MissingColumn(c) => write!(f, "record batch is missing the `{c}` column"),
			Self::WrongType { column, expected } => write!(f, "column `{column}` is not the expected Arrow {expected}"),
			Self::MissingMetadata(k) => write!(f, "record batch schema is missing required metadata `{k}`"),
			Self::UnknownTimeUnit(s) => write!(f, "unrecognized time unit `{s}` in schema metadata"),
			Self::BadValue(s) => write!(f, "value cell `{s}` does not parse as a decimal"),
			Self::Segment(e) => write!(f, "could not rebuild segment from columns: {e}"),
		}
	}
}

impl std::error::Error for ConvertError {}

impl From<SegmentError> for ConvertError {
	fn from(e: SegmentError) -> Self {
		Self::Segment(e)
	}
}

/// Map a [`TimeUnit`] name (as written by [`TimeUnit::name`]) back to the variant.
#[must_use]
fn time_unit_from_name(name: &str) -> Option<TimeUnit> {
	match name {
		"seconds" => Some(TimeUnit::Seconds),
		"millis" => Some(TimeUnit::Millis),
		"micros" => Some(TimeUnit::Micros),
		"nanos" => Some(TimeUnit::Nanos),
		_ => None,
	}
}

/// Convert a sealed [`Segment`] into a self-describing, lossless Arrow
/// [`RecordBatch`].
///
/// The batch has two columns: a non-null `timestamp: Int64` (the integer epochs,
/// exactly as stored) and a nullable `value: Utf8` (each present value's plain
/// decimal text, Arrow validity marking the absent rows). The segment's
/// [`TimeUnit`], physical-encoding name, value wire form, and format version are
/// recorded in the schema metadata so [`segment_from_record_batch`] can rebuild
/// it without out-of-band information.
///
/// The conversion is lossless: decimal text preserves every digit, so no value is
/// downcast crossing into Arrow.
///
/// # Panics
///
/// Never in practice: both columns are built from the segment's single row count,
/// so the internal [`RecordBatch::try_new`] cannot see a column-length mismatch. A
/// panic here would mean a bug in this function, not bad input.
#[must_use]
pub fn segment_to_record_batch(seg: &Segment) -> RecordBatch {
	let (timestamps, values) = seg.decode_nullable();
	let val_array: ArrayRef = Arc::new(values.iter().map(|opt| opt.as_ref().map(BigDecimal::to_plain_string)).collect::<StringArray>());
	assemble_batch(seg, &timestamps, val_array, VALUE_ENCODING_TEXT)
}

/// Convert a sealed [`Segment`] into an Arrow [`RecordBatch`] using the **typed
/// numeric fast path** when the segment's physical encoding has a natural
/// fixed-width Arrow array.
///
/// An [`F64`](PhysicalType::F64) segment emits an Arrow `Float64` value column and
/// an [`F32`](PhysicalType::F32) segment an Arrow `Float32` column — the layout
/// SIMD/GPU consumers (`DataFusion`, `pandas`, Flight) expect, half the bytes of
/// decimal text for F32. Every other encoding (the scaled-integer / decimal /
/// text types, whose faithful Arrow form is a uniform-scale `Decimal128` a later
/// slice will add) falls back to the lossless [`segment_to_record_batch`] text
/// column, so the export is always correct, just not always the narrowest.
///
/// The schema metadata records which form was chosen
/// ([`META_VALUE_ENCODING`] = `f64` / `f32` / `text`) so
/// [`record_batch_to_columns`] reads it back without guessing. The numeric path is
/// exact: an F64 segment already *stores* its values as `f64`, so emitting them as
/// Arrow `Float64` reproduces the stored bits — no second downcast.
#[must_use]
pub fn segment_to_record_batch_typed(seg: &Segment) -> RecordBatch {
	let (timestamps, values) = seg.decode_nullable();
	match seg.physical_type() {
		PhysicalType::F64 => {
			let val_array: ArrayRef = Arc::new(values.iter().map(|opt| opt.as_ref().and_then(BigDecimal::to_f64)).collect::<Float64Array>());
			assemble_batch(seg, &timestamps, val_array, VALUE_ENCODING_F64)
		}
		PhysicalType::F32 => {
			let val_array: ArrayRef = Arc::new(values.iter().map(|opt| opt.as_ref().and_then(BigDecimal::to_f32)).collect::<Float32Array>());
			assemble_batch(seg, &timestamps, val_array, VALUE_ENCODING_F32)
		}
		_ => segment_to_record_batch(seg),
	}
}

/// Assemble the two-column batch from a prepared value array, recording the
/// self-describing schema metadata. Shared by the text and typed export paths.
///
/// # Panics
///
/// Never in practice: the timestamp and value arrays are both built from the
/// segment's single row count, so the internal [`RecordBatch::try_new`] cannot see
/// a column-length mismatch. A panic here would mean a bug in this crate, not bad
/// input.
fn assemble_batch(seg: &Segment, timestamps: &[i64], val_array: ArrayRef, value_encoding: &str) -> RecordBatch {
	let ts_array = Int64Array::from_iter_values(timestamps.iter().copied());
	let value_type = val_array.data_type().clone();

	let mut metadata = HashMap::new();
	metadata.insert(META_TIME_UNIT.to_string(), seg.time_unit().name().to_string());
	metadata.insert(META_PHYSICAL_TYPE.to_string(), seg.physical_type().name().to_string());
	metadata.insert(META_VALUE_ENCODING.to_string(), value_encoding.to_string());
	metadata.insert(META_FORMAT_VERSION.to_string(), seg.version.to_string());

	let schema = Schema::new_with_metadata(vec![Field::new(TIMESTAMP_COLUMN, DataType::Int64, false), Field::new(VALUE_COLUMN, value_type, true)], metadata);

	RecordBatch::try_new(Arc::new(schema), vec![Arc::new(ts_array), val_array]).expect("timestamp and value columns share the segment row count")
}

/// Read an exported [`RecordBatch`] back into DSP's logical columns.
///
/// Returns the dense timestamp column and the value column aligned to it, with
/// [`None`] at every Arrow-null (absent) row — the inverse shape of
/// [`Segment::decode_nullable`].
///
/// Handles both export forms: a `Utf8` value column ([`segment_to_record_batch`])
/// is parsed from decimal text, a `Float64` / `Float32` column
/// ([`segment_to_record_batch_typed`]) is lifted back to `BigDecimal`. Dispatch is
/// on the column's actual Arrow type, so a batch produced by either path round
/// trips.
///
/// # Errors
///
/// Returns a [`ConvertError`] if a required column is missing, the value column is
/// an Arrow type this crate does not emit, or a text cell does not parse as a
/// `BigDecimal`.
pub fn record_batch_to_columns(batch: &RecordBatch) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>), ConvertError> {
	let ts_col = batch.column_by_name(TIMESTAMP_COLUMN).ok_or(ConvertError::MissingColumn(TIMESTAMP_COLUMN))?;
	let ts_array = ts_col.as_any().downcast_ref::<Int64Array>().ok_or(ConvertError::WrongType { column: TIMESTAMP_COLUMN, expected: "Int64" })?;
	let timestamps: Vec<i64> = ts_array.values().to_vec();

	let val_col = batch.column_by_name(VALUE_COLUMN).ok_or(ConvertError::MissingColumn(VALUE_COLUMN))?;
	let values = match val_col.data_type() {
		DataType::Utf8 => {
			let val_array = val_col.as_any().downcast_ref::<StringArray>().ok_or(ConvertError::WrongType { column: VALUE_COLUMN, expected: "Utf8" })?;
			let mut out = Vec::with_capacity(val_array.len());
			for cell in val_array {
				match cell {
					Some(text) => out.push(Some(text.parse::<BigDecimal>().map_err(|_| ConvertError::BadValue(text.to_string()))?)),
					None => out.push(None),
				}
			}
			out
		}
		DataType::Float64 => {
			let val_array = val_col.as_any().downcast_ref::<Float64Array>().ok_or(ConvertError::WrongType { column: VALUE_COLUMN, expected: "Float64" })?;
			val_array.iter().map(|cell| cell.and_then(BigDecimal::from_f64)).collect()
		}
		DataType::Float32 => {
			let val_array = val_col.as_any().downcast_ref::<Float32Array>().ok_or(ConvertError::WrongType { column: VALUE_COLUMN, expected: "Float32" })?;
			val_array.iter().map(|cell| cell.and_then(BigDecimal::from_f32)).collect()
		}
		_ => return Err(ConvertError::WrongType { column: VALUE_COLUMN, expected: "Utf8, Float64, or Float32" }),
	};
	Ok((timestamps, values))
}

/// Rebuild a [`Segment`] from an exported [`RecordBatch`].
///
/// Recovers the [`TimeUnit`] from the schema metadata, reads the columns via
/// [`record_batch_to_columns`], and re-seals them with
/// [`Segment::build_nullable`] under `value_tolerance` (`0` for a fully lossless
/// segment — appropriate when the batch came from
/// [`segment_to_record_batch`], whose value column is exact decimal text).
///
/// # Errors
///
/// Returns a [`ConvertError`] if the time-unit metadata is missing or
/// unrecognized, a column is malformed (see [`record_batch_to_columns`]), or the
/// reconstructed columns cannot form a segment.
pub fn segment_from_record_batch(batch: &RecordBatch, value_tolerance: &BigDecimal) -> Result<Segment, ConvertError> {
	let unit_name = batch.schema_ref().metadata().get(META_TIME_UNIT).ok_or(ConvertError::MissingMetadata(META_TIME_UNIT))?.clone();
	let unit = time_unit_from_name(&unit_name).ok_or(ConvertError::UnknownTimeUnit(unit_name))?;
	let (timestamps, values) = record_batch_to_columns(batch)?;
	Ok(Segment::build_nullable(&timestamps, &values, unit, value_tolerance)?)
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use dsp_physical_type::AspectSchema;

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("test literal parses")
	}

	fn col(lits: &[&str]) -> Vec<BigDecimal> {
		lits.iter().map(|s| bd(s)).collect()
	}

	fn ncol(lits: &[Option<&str>]) -> Vec<Option<BigDecimal>> {
		lits.iter().map(|o| o.map(bd)).collect()
	}

	#[test]
	fn dense_segment_round_trips_through_arrow() {
		let timestamps: Vec<i64> = (0..5).map(|i| 1_000 + i * 10).collect();
		let values = col(&["1.25", "2.50", "3.75", "5.00", "6.25"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Micros, &bd("0")).expect("builds");

		let batch = segment_to_record_batch(&seg);
		assert_eq!(batch.num_columns(), 2);
		assert_eq!(batch.num_rows(), 5);

		let back = segment_from_record_batch(&batch, &bd("0")).expect("rebuilds");
		assert_eq!(back.decode(), (timestamps, values));
		assert_eq!(back.time_unit(), TimeUnit::Micros);
	}

	#[test]
	fn nullable_segment_preserves_gaps_as_arrow_nulls() {
		let timestamps = vec![10_i64, 20, 30, 40, 50];
		let values = ncol(&[Some("1.5"), None, Some("3.5"), None, Some("5.5")]);
		let seg = Segment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &bd("0")).expect("builds");

		let batch = segment_to_record_batch(&seg);
		// The Arrow value column carries the two nulls as real validity bits.
		let val_array = batch.column_by_name(VALUE_COLUMN).unwrap().as_any().downcast_ref::<StringArray>().unwrap();
		assert_eq!(val_array.null_count(), 2);
		assert!(val_array.is_null(1) && val_array.is_null(3));

		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn high_precision_values_survive_losslessly() {
		// Far more digits than f64 or i128 could hold — text keeps every one.
		let timestamps = vec![1_i64, 2];
		let values = col(&["123456789012345678901234567890.123456789012345678901234567890", "-0.000000000000000000000000000001"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Nanos, &bd("0")).expect("builds");
		assert!(seg.is_exact());

		let batch = segment_to_record_batch(&seg);
		let back = segment_from_record_batch(&batch, &bd("0")).expect("rebuilds");
		assert!(back.is_exact(), "decimal text crossing into Arrow loses nothing");
		assert_eq!(back.decode_values(), values);
	}

	#[test]
	fn schema_metadata_is_self_describing() {
		let seg = Segment::build(&[5_i64], &col(&["7.5"]), TimeUnit::Millis, &bd("0")).expect("builds");
		let batch = segment_to_record_batch(&seg);
		let md = batch.schema_ref().metadata();
		assert_eq!(md.get(META_TIME_UNIT).map(String::as_str), Some("millis"));
		assert_eq!(md.get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_TEXT));
		assert_eq!(md.get(META_FORMAT_VERSION).map(String::as_str), Some(&seg.version.to_string()[..]));
		assert!(md.contains_key(META_PHYSICAL_TYPE));
	}

	#[test]
	fn empty_segment_round_trips() {
		let seg = Segment::build(&[], &[], TimeUnit::Seconds, &bd("0")).expect("builds");
		let batch = segment_to_record_batch(&seg);
		assert_eq!(batch.num_rows(), 0);
		let back = segment_from_record_batch(&batch, &bd("0")).expect("rebuilds");
		assert!(back.is_empty());
	}

	#[test]
	fn missing_time_unit_metadata_is_reported() {
		// A batch with the right columns but no metadata cannot become a segment.
		let ts = Int64Array::from(vec![1_i64, 2]);
		let vals = StringArray::from(vec![Some("1.0"), Some("2.0")]);
		let schema = Schema::new(vec![Field::new(TIMESTAMP_COLUMN, DataType::Int64, false), Field::new(VALUE_COLUMN, DataType::Utf8, true)]);
		let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(ts), Arc::new(vals)]).expect("constructs");
		assert_eq!(segment_from_record_batch(&batch, &bd("0")), Err(ConvertError::MissingMetadata(META_TIME_UNIT)));
	}

	#[test]
	fn missing_column_is_reported() {
		let schema = Schema::new(vec![Field::new(TIMESTAMP_COLUMN, DataType::Int64, false)]);
		let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(Int64Array::from(vec![1_i64]))]).expect("constructs");
		assert_eq!(record_batch_to_columns(&batch), Err(ConvertError::MissingColumn(VALUE_COLUMN)));
	}

	#[test]
	fn unparseable_value_cell_is_reported() {
		let ts = Int64Array::from(vec![1_i64]);
		let vals = StringArray::from(vec![Some("not-a-number")]);
		let schema = Schema::new(vec![Field::new(TIMESTAMP_COLUMN, DataType::Int64, false), Field::new(VALUE_COLUMN, DataType::Utf8, true)]);
		let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(ts), Arc::new(vals)]).expect("constructs");
		assert_eq!(record_batch_to_columns(&batch), Err(ConvertError::BadValue("not-a-number".to_string())));
	}

	#[test]
	fn f64_segment_uses_arrow_float64_and_round_trips() {
		// Values exact in binary64, sealed under a declared F64 encoding.
		let timestamps = vec![1_i64, 2, 3, 4];
		let values = col(&["0.5", "2.25", "-0.25", "128"]);
		let seg = AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds).seal(&timestamps, &values).expect("seals");
		assert_eq!(seg.physical_type(), PhysicalType::F64);

		let batch = segment_to_record_batch_typed(&seg);
		// The value column is a real Arrow Float64, not text.
		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Float64);
		assert_eq!(batch.schema_ref().metadata().get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_F64));

		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values.into_iter().map(Some).collect::<Vec<_>>());
	}

	#[test]
	fn f32_segment_uses_arrow_float32_and_round_trips() {
		let timestamps = vec![10_i64, 20, 30];
		let values = col(&["0.5", "0.25", "-4"]);
		let seg = AspectSchema::new(PhysicalType::F32, bd("0"), TimeUnit::Millis).seal(&timestamps, &values).expect("seals");
		assert_eq!(seg.physical_type(), PhysicalType::F32);

		let batch = segment_to_record_batch_typed(&seg);
		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Float32);
		assert_eq!(batch.schema_ref().metadata().get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_F32));

		let back = segment_from_record_batch(&batch, &bd("0")).expect("rebuilds");
		assert_eq!(back.decode(), (timestamps, values));
	}

	#[test]
	fn typed_export_preserves_nulls_in_the_float_column() {
		let timestamps = vec![1_i64, 2, 3, 4, 5];
		let values = ncol(&[Some("0.5"), None, Some("2.25"), None, Some("-8")]);
		let seg = AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds).seal_nullable(&timestamps, &values).expect("seals");

		let batch = segment_to_record_batch_typed(&seg);
		let val_array = batch.column_by_name(VALUE_COLUMN).unwrap().as_any().downcast_ref::<Float64Array>().unwrap();
		assert_eq!(val_array.null_count(), 2);
		assert!(val_array.is_null(1) && val_array.is_null(3));

		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn typed_export_falls_back_to_text_for_non_float_encodings() {
		// A 60-digit value can only be held losslessly by the text encoding, so the
		// typed path must fall back to Utf8 rather than downcast it to a float.
		let timestamps = vec![1_i64, 2];
		let values = col(&["123456789012345678901234567890.123456789012345678901234567890", "1"]);
		let seg = Segment::build(&timestamps, &values, TimeUnit::Nanos, &bd("0")).expect("builds");
		assert_eq!(seg.physical_type(), PhysicalType::BigDecimalText);

		let batch = segment_to_record_batch_typed(&seg);
		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Utf8);
		assert_eq!(batch.schema_ref().metadata().get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_TEXT));
		let back = segment_from_record_batch(&batch, &bd("0")).expect("rebuilds");
		assert!(back.is_exact());
		assert_eq!(back.decode_values(), values);
	}
}
