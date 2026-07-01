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

use arrow_array::{Array, ArrayRef, Decimal128Array, Float32Array, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_ipc::{reader::StreamReader, writer::StreamWriter};
use arrow_schema::{DataType, Field, Schema, DECIMAL128_MAX_PRECISION};
use bytes::Bytes;
use parquet::arrow::{arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter};
use bigdecimal::{num_bigint::BigInt, BigDecimal, FromPrimitive, ToPrimitive};
use dsp_physical_type::{Page, PagedSegment, PhysicalType, Segment, SegmentError, TimeUnit};

/// Name of the timestamp column in an exported [`RecordBatch`].
pub const TIMESTAMP_COLUMN: &str = "timestamp";
/// Name of the value column in an exported [`RecordBatch`].
pub const VALUE_COLUMN: &str = "value";
/// Name of the provenance column in a **reconstructed-series** batch.
///
/// Built by [`reconstructed_series_to_record_batch`]; each cell is `raw` /
/// `interpolated` / `extrapolated`, so a consumer never silently treats a
/// synthetic point as an observed one (backlog item B-tags).
pub const SERIES_KIND_COLUMN: &str = "kind";
/// Name of the bucket-sample-count column in a **reduction-table** batch
/// ([`reduction_table_to_record_batch`]).
pub const REDUCTION_COUNT_COLUMN: &str = "count";

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
/// Value-column wire form: fixed-scale Arrow `Decimal128`.
///
/// The typed, **exact** fast path for the fixed-scale
/// [`ScaledI64`](dsp_physical_type::PhysicalType::ScaledI64) /
/// [`ScaledI128`](dsp_physical_type::PhysicalType::ScaledI128) encodings.
pub const VALUE_ENCODING_DECIMAL128: &str = "decimal128";

/// The columns recovered from a reconstructed-series batch by
/// [`reconstructed_series_from_record_batch`]: `(time unit, timestamps, values,
/// provenance kinds)`.
pub type ReconstructedSeries = (TimeUnit, Vec<i64>, Vec<f64>, Vec<String>);

/// The columns recovered from a reduction-table batch by
/// [`reduction_table_from_record_batch`]: `(time unit, bucket timestamps, per-bucket
/// counts, named reduction columns)`.
pub type ReductionTable = (TimeUnit, Vec<i64>, Vec<i64>, Vec<(String, Vec<f64>)>);

/// Format-version sentinel written into [`META_FORMAT_VERSION`] for a batch built
/// from **logical columns** rather than a single sealed segment.
///
/// A time- or value-range read of the segment store
/// ([`columns_to_record_batch`]) can span many `.dspseg` segments of differing
/// format versions and physical encodings, so no single
/// [`SEGMENT_FORMAT_VERSION`](dsp_physical_type::Segment::version) applies. `0`
/// marks the batch as a logical-column export (the lossless decimal-text form),
/// distinct from any real on-disk segment version (which start at 1).
pub const LOGICAL_EXPORT_VERSION: u16 = 0;

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
	/// Reading or writing the Arrow IPC stream failed (carries the Arrow message).
	Ipc(String),
	/// An IPC or Parquet write was asked for an empty batch set — there is no
	/// schema to write.
	EmptyBatchSet,
	/// Reading or writing the Parquet file failed (carries the Parquet message).
	Parquet(String),
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
			Self::Ipc(e) => write!(f, "arrow IPC stream error: {e}"),
			Self::EmptyBatchSet => write!(f, "cannot write from zero batches (no schema)"),
			Self::Parquet(e) => write!(f, "parquet error: {e}"),
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

/// Build a self-describing, lossless Arrow [`RecordBatch`] directly from DSP's
/// **logical columns** — a dense `timestamp: Int64` column and a value column
/// aligned to it, with [`None`] at each absent row.
///
/// This is the column-level entry point used when the source is *not* a single
/// sealed [`Segment`] but a logical read that may have crossed several segments —
/// for example a time- or value-range read of the segment store, whose result is
/// exactly a `(Vec<i64>, Vec<Option<BigDecimal>>)`. It emits the same two-column
/// shape as [`segment_to_record_batch`] (non-null `timestamp: Int64`, nullable
/// `value: Utf8` plain decimal text), so [`record_batch_to_columns`] reads it back
/// unchanged.
///
/// Because the columns may span multiple physical encodings, the metadata records
/// the lossless logical view rather than any one segment's: the physical-type name
/// is [`PhysicalType::BigDecimalText`] (the value column *is* decimal text) and the
/// format version is [`LOGICAL_EXPORT_VERSION`]. The conversion is lossless — no
/// value loses a digit crossing into Arrow (hard constraint #4).
///
/// # Panics
///
/// Panics if `timestamps` and `values` differ in length: the two Arrow columns
/// must share a row count. Callers pass columns that are already aligned (the
/// segment-store readers always do), so this signals a caller bug, not bad data.
#[must_use]
pub fn columns_to_record_batch(unit: TimeUnit, timestamps: &[i64], values: &[Option<BigDecimal>]) -> RecordBatch {
	assert_eq!(timestamps.len(), values.len(), "timestamp and value columns must share a row count");
	let (val_array, value_encoding) = text_value_array(values);
	build_record_batch(unit, PhysicalType::BigDecimalText.name(), LOGICAL_EXPORT_VERSION, timestamps, val_array, value_encoding)
}

/// Build a self-describing Arrow [`RecordBatch`] from DSP's logical columns using
/// the **typed numeric fast path** for a given physical encoding.
///
/// The column-level counterpart of [`segment_to_record_batch_typed`].
/// Where [`columns_to_record_batch`] always emits the lossless decimal-text value
/// column, this emits the natural fixed-width Arrow array for `physical_type` when
/// one exists: [`F64`](PhysicalType::F64) → `Float64`, [`F32`](PhysicalType::F32) →
/// `Float32`, and the fixed-scale [`ScaledI64`](PhysicalType::ScaledI64) /
/// [`ScaledI128`](PhysicalType::ScaledI128) → an **exact** `Decimal128`. Any other
/// encoding (and any fixed-scale value that overflows the Arrow precision bound)
/// falls back to the lossless text column, so the export is always correct.
///
/// This is the form a typed stored-range read uses: an aspect declares **one**
/// physical encoding for all its segments
/// ([`AspectSchema::value`](dsp_physical_type::AspectSchema)), so the whole logical
/// range shares it and a single typed Arrow column is well-defined. The numeric
/// path is exact, not a second downcast — values stored under `F64` already *are*
/// `f64`. [`record_batch_to_columns`] reads either form back by dispatching on the
/// value column's Arrow type.
///
/// # Panics
///
/// Panics if `timestamps` and `values` differ in length (see
/// [`columns_to_record_batch`]).
#[must_use]
pub fn columns_to_record_batch_typed(unit: TimeUnit, physical_type: PhysicalType, timestamps: &[i64], values: &[Option<BigDecimal>]) -> RecordBatch {
	assert_eq!(timestamps.len(), values.len(), "timestamp and value columns must share a row count");
	let (val_array, value_encoding) = typed_value_array(physical_type, values);
	build_record_batch(unit, physical_type.name(), LOGICAL_EXPORT_VERSION, timestamps, val_array, value_encoding)
}

/// Convert a sealed [`Segment`] into an Arrow [`RecordBatch`] using the **typed
/// numeric fast path** when the segment's physical encoding has a natural
/// fixed-width Arrow array.
///
/// An [`F64`](PhysicalType::F64) segment emits an Arrow `Float64` value column and
/// an [`F32`](PhysicalType::F32) segment an Arrow `Float32` column — the layout
/// SIMD/GPU consumers (`DataFusion`, `pandas`, Flight) expect, half the bytes of
/// decimal text for F32. The fixed-scale
/// [`ScaledI64`](PhysicalType::ScaledI64) / [`ScaledI128`](PhysicalType::ScaledI128)
/// encodings emit an **exact** Arrow `Decimal128` column at the encoding's scale —
/// no float rounding at all. The per-value-scale
/// [`Decimal128`](PhysicalType::Decimal128) and the variable-width
/// [`BigDecimalText`](PhysicalType::BigDecimalText) fall back to the lossless
/// [`segment_to_record_batch`] text column (a single Arrow `Decimal128` column
/// needs one shared scale, which the per-value encoding does not have), so the
/// export is always correct, just not always the narrowest.
///
/// The schema metadata records which form was chosen
/// ([`META_VALUE_ENCODING`] = `f64` / `f32` / `text`) so
/// [`record_batch_to_columns`] reads it back without guessing. The numeric path is
/// exact: an F64 segment already *stores* its values as `f64`, so emitting them as
/// Arrow `Float64` reproduces the stored bits — no second downcast.
#[must_use]
pub fn segment_to_record_batch_typed(seg: &Segment) -> RecordBatch {
	let (timestamps, values) = seg.decode_nullable();
	let (val_array, value_encoding) = typed_value_array(seg.physical_type(), &values);
	assemble_batch(seg, &timestamps, val_array, value_encoding)
}

/// Build the **typed** Arrow value array for a value column under a given physical
/// encoding, returning the array and the [`META_VALUE_ENCODING`] tag describing it.
///
/// [`F64`](PhysicalType::F64) → `Float64`, [`F32`](PhysicalType::F32) → `Float32`,
/// and the fixed-scale [`ScaledI64`](PhysicalType::ScaledI64) /
/// [`ScaledI128`](PhysicalType::ScaledI128) → an **exact** `Decimal128` at the
/// encoding's scale. Any other encoding — and any fixed-scale column that overflows
/// `i128`/the Arrow precision bound — falls back to the always-correct lossless
/// text array ([`text_value_array`]) rather than emit a wrong number.
///
/// Shared by [`segment_to_record_batch_typed`] (driven by a sealed segment's
/// encoding) and [`columns_to_record_batch_typed`] (driven by an aspect's declared
/// schema encoding), so both produce identical typed columns.
fn typed_value_array(physical_type: PhysicalType, values: &[Option<BigDecimal>]) -> (ArrayRef, &'static str) {
	match physical_type {
		PhysicalType::F64 => (Arc::new(values.iter().map(|opt| opt.as_ref().and_then(BigDecimal::to_f64)).collect::<Float64Array>()), VALUE_ENCODING_F64),
		PhysicalType::F32 => (Arc::new(values.iter().map(|opt| opt.as_ref().and_then(BigDecimal::to_f32)).collect::<Float32Array>()), VALUE_ENCODING_F32),
		// Exact: every value carries at most `scale` fractional digits, so its mantissa
		// at that scale is an exact integer. On overflow, fall back to lossless text.
		PhysicalType::ScaledI64 { scale } | PhysicalType::ScaledI128 { scale } => try_decimal128_array(values, scale).map_or_else(|| text_value_array(values), |arr| (arr, VALUE_ENCODING_DECIMAL128)),
		_ => text_value_array(values),
	}
}

/// Build the lossless plain-decimal-text Arrow value array (Arrow `Utf8`, validity
/// marking absent rows) and its [`VALUE_ENCODING_TEXT`] tag — the always-exact
/// fallback for any encoding without a natural fixed-width Arrow array.
fn text_value_array(values: &[Option<BigDecimal>]) -> (ArrayRef, &'static str) {
	(Arc::new(values.iter().map(|opt| opt.as_ref().map(BigDecimal::to_plain_string)).collect::<StringArray>()), VALUE_ENCODING_TEXT)
}

/// Build an exact Arrow `Decimal128` array for a fixed-scale value column, or
/// [`None`] if any present value cannot be represented exactly at that scale
/// within `i128` and the Arrow precision bound (signalling the caller to fall back
/// to the lossless text form).
fn try_decimal128_array(values: &[Option<BigDecimal>], scale: u8) -> Option<ArrayRef> {
	let scale_i64 = i64::from(scale);
	let mut mantissas: Vec<Option<i128>> = Vec::with_capacity(values.len());
	for opt in values {
		match opt {
			Some(bd) => mantissas.push(Some(bd.clone().with_scale(scale_i64).into_bigint_and_exponent().0.to_i128()?)),
			None => mantissas.push(None),
		}
	}
	let scale_i8 = i8::try_from(scale).ok()?;
	let array = Decimal128Array::from(mantissas).with_precision_and_scale(DECIMAL128_MAX_PRECISION, scale_i8).ok()?;
	Some(Arc::new(array))
}

/// Assemble the two-column batch from a prepared value array, reading the
/// self-describing schema metadata off a [`Segment`]. Shared by the text and typed
/// segment-export paths.
fn assemble_batch(seg: &Segment, timestamps: &[i64], val_array: ArrayRef, value_encoding: &str) -> RecordBatch {
	build_record_batch(seg.time_unit(), seg.physical_type().name(), seg.version, timestamps, val_array, value_encoding)
}

/// Assemble the two-column batch from explicit metadata fields and a prepared
/// value array — the shared core under both the [`Segment`] and [`Page`] export
/// paths.
///
/// # Panics
///
/// Never in practice: the timestamp and value arrays are both built from the same
/// row count, so the internal [`RecordBatch::try_new`] cannot see a column-length
/// mismatch. A panic here would mean a bug in this crate, not bad input.
fn build_record_batch(unit: TimeUnit, physical_type_name: &str, version: u16, timestamps: &[i64], val_array: ArrayRef, value_encoding: &str) -> RecordBatch {
	let ts_array = Int64Array::from_iter_values(timestamps.iter().copied());
	let value_type = val_array.data_type().clone();

	let mut metadata = HashMap::new();
	metadata.insert(META_TIME_UNIT.to_string(), unit.name().to_string());
	metadata.insert(META_PHYSICAL_TYPE.to_string(), physical_type_name.to_string());
	metadata.insert(META_VALUE_ENCODING.to_string(), value_encoding.to_string());
	metadata.insert(META_FORMAT_VERSION.to_string(), version.to_string());

	let schema = Schema::new_with_metadata(vec![Field::new(TIMESTAMP_COLUMN, DataType::Int64, false), Field::new(VALUE_COLUMN, value_type, true)], metadata);

	RecordBatch::try_new(Arc::new(schema), vec![Arc::new(ts_array), val_array]).expect("timestamp and value columns share the row count")
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
		DataType::Decimal128(_precision, scale) => {
			let val_array = val_col.as_any().downcast_ref::<Decimal128Array>().ok_or(ConvertError::WrongType { column: VALUE_COLUMN, expected: "Decimal128" })?;
			let scale_i64 = i64::from(*scale);
			// Exact inverse of the export: mantissa * 10^(-scale) reconstructs the
			// BigDecimal with no loss.
			val_array.iter().map(|cell| cell.map(|mantissa| BigDecimal::new(BigInt::from(mantissa), scale_i64))).collect()
		}
		_ => return Err(ConvertError::WrongType { column: VALUE_COLUMN, expected: "Utf8, Float64, Float32, or Decimal128" }),
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
	let unit = unit_from_metadata(batch)?;
	let (timestamps, values) = record_batch_to_columns(batch)?;
	Ok(Segment::build_nullable(&timestamps, &values, unit, value_tolerance)?)
}

/// Read and validate the [`TimeUnit`] recorded in a batch's schema metadata.
fn unit_from_metadata(batch: &RecordBatch) -> Result<TimeUnit, ConvertError> {
	let unit_name = batch.schema_ref().metadata().get(META_TIME_UNIT).ok_or(ConvertError::MissingMetadata(META_TIME_UNIT))?;
	time_unit_from_name(unit_name).ok_or_else(|| ConvertError::UnknownTimeUnit(unit_name.clone()))
}

/// Export a [`PagedSegment`] as a **stream of Arrow [`RecordBatch`]es, one per
/// page** — the Arrow-idiomatic representation of a paged column.
///
/// Each page becomes one lossless two-column batch (`timestamp: Int64`,
/// `value: Utf8`) carrying the page's own [`TimeUnit`] and physical-encoding name
/// in its schema metadata. Preserving the page boundary keeps the Phase-4.4
/// page-skipping structure visible to a consumer: it can prune a page on its
/// stats and never materialize the others. An empty paged segment (no pages)
/// yields an empty vector.
///
/// To collapse the whole segment into a single batch instead, use
/// [`paged_segment_to_record_batch`].
#[must_use]
pub fn paged_segment_to_record_batches(seg: &PagedSegment) -> Vec<RecordBatch> {
	seg.pages.iter().map(|page| page_to_record_batch(page, seg.version)).collect()
}

/// Export a [`PagedSegment`] as a **single** lossless Arrow [`RecordBatch`],
/// collapsing every page into one two-column batch.
///
/// Convenient when a consumer wants the whole aspect at once and does not need the
/// page boundaries (e.g. a one-shot `pyarrow` hand-off). Page-level pruning is lost
/// in this form; use [`paged_segment_to_record_batches`] to keep it. The segment's
/// shared [`TimeUnit`] is taken from its first page; an empty segment produces an
/// empty `seconds`-unit batch.
#[must_use]
pub fn paged_segment_to_record_batch(seg: &PagedSegment) -> RecordBatch {
	let (timestamps, values) = seg.decode_nullable();
	let unit = seg.pages.first().map_or(TimeUnit::Seconds, |p| p.timestamps.unit);
	let physical_type_name = seg.pages.first().map_or(PhysicalType::BigDecimalText.name(), |p| p.values.physical_type.name());
	let val_array: ArrayRef = Arc::new(values.iter().map(|opt| opt.as_ref().map(BigDecimal::to_plain_string)).collect::<StringArray>());
	build_record_batch(unit, physical_type_name, seg.version, &timestamps, val_array, VALUE_ENCODING_TEXT)
}

/// Build a lossless per-page batch from a [`Page`] and the owning segment's
/// format version.
fn page_to_record_batch(page: &Page, version: u16) -> RecordBatch {
	let (timestamps, values) = page.decode_nullable();
	let val_array: ArrayRef = Arc::new(values.iter().map(|opt| opt.as_ref().map(BigDecimal::to_plain_string)).collect::<StringArray>());
	build_record_batch(page.timestamps.unit, page.values.physical_type.name(), version, &timestamps, val_array, VALUE_ENCODING_TEXT)
}

/// Concatenate a stream of exported [`RecordBatch`]es back into one pair of DSP
/// logical columns, in order.
///
/// The inverse of [`paged_segment_to_record_batches`] at the column level: each
/// batch is read with [`record_batch_to_columns`] and appended. Batches may mix
/// the text and typed value forms (the per-batch dispatch handles each).
///
/// # Errors
///
/// Propagates any [`ConvertError`] from reading an individual batch.
pub fn record_batches_to_columns(batches: &[RecordBatch]) -> Result<(Vec<i64>, Vec<Option<BigDecimal>>), ConvertError> {
	let mut timestamps = Vec::new();
	let mut values = Vec::new();
	for batch in batches {
		let (ts, vs) = record_batch_to_columns(batch)?;
		timestamps.extend(ts);
		values.extend(vs);
	}
	Ok((timestamps, values))
}

/// Rebuild a [`PagedSegment`] from a stream of exported per-page [`RecordBatch`]es.
///
/// Recovers the shared [`TimeUnit`] from the first batch's metadata, concatenates
/// the pages' columns via [`record_batches_to_columns`], and re-partitions them
/// into pages of `rows_per_page` under `value_tolerance`. The page boundaries of
/// the result depend on `rows_per_page`, not on how the input batches were chunked
/// — so this round-trips [`paged_segment_to_record_batches`] when given the
/// original segment's [`rows_per_page`](PagedSegment::rows_per_page).
///
/// # Errors
///
/// Returns [`ConvertError::MissingMetadata`] if `batches` is empty (no batch to
/// read the [`TimeUnit`] from — build an empty segment directly instead), or any
/// error from reading a batch or rebuilding the segment.
pub fn paged_segment_from_record_batches(batches: &[RecordBatch], value_tolerance: &BigDecimal, rows_per_page: usize) -> Result<PagedSegment, ConvertError> {
	let first = batches.first().ok_or(ConvertError::MissingMetadata(META_TIME_UNIT))?;
	let unit = unit_from_metadata(first)?;
	let (timestamps, values) = record_batches_to_columns(batches)?;
	Ok(PagedSegment::build_nullable(&timestamps, &values, unit, value_tolerance, rows_per_page)?)
}

/// Serialize one or more [`RecordBatch`]es into the **Arrow IPC stream** wire format
/// — the standard byte representation an HTTP body, an Arrow Flight payload, or a
/// `.arrow` file carries.
///
/// All batches must share the schema of the first (they do when they come from the
/// same export — every page batch of a [`PagedSegment`] carries identical schema
/// metadata). The self-describing schema (including DSP's [`META_TIME_UNIT`] and
/// physical-encoding metadata) is written once at the head of the stream, so
/// [`read_ipc_stream`] reconstructs the batches — and DSP's columns from them —
/// with no out-of-band information.
///
/// # Errors
///
/// Returns [`ConvertError::EmptyBatchSet`] if `batches` is empty (no schema to
/// write), or [`ConvertError::Ipc`] if the Arrow writer fails.
pub fn write_ipc_stream(batches: &[RecordBatch]) -> Result<Vec<u8>, ConvertError> {
	let schema = batches.first().ok_or(ConvertError::EmptyBatchSet)?.schema();
	let mut buf = Vec::new();
	let mut writer = StreamWriter::try_new(&mut buf, &schema).map_err(|e| ConvertError::Ipc(e.to_string()))?;
	for batch in batches {
		writer.write(batch).map_err(|e| ConvertError::Ipc(e.to_string()))?;
	}
	writer.finish().map_err(|e| ConvertError::Ipc(e.to_string()))?;
	drop(writer);
	Ok(buf)
}

/// Deserialize an **Arrow IPC stream** (as written by [`write_ipc_stream`]) back
/// into its [`RecordBatch`]es, in order.
///
/// The inverse of [`write_ipc_stream`]. Feed the result to
/// [`record_batch_to_columns`] / [`record_batches_to_columns`] (or
/// [`segment_from_record_batch`]) to recover DSP's columns.
///
/// # Errors
///
/// Returns [`ConvertError::Ipc`] if the bytes are not a valid Arrow IPC stream or a
/// batch fails to decode.
pub fn read_ipc_stream(bytes: &[u8]) -> Result<Vec<RecordBatch>, ConvertError> {
	let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).map_err(|e| ConvertError::Ipc(e.to_string()))?;
	let mut batches = Vec::new();
	for batch in reader {
		batches.push(batch.map_err(|e| ConvertError::Ipc(e.to_string()))?);
	}
	Ok(batches)
}

/// Serialize one or more [`RecordBatch`]es into an Apache **Parquet** file's bytes.
///
/// Parquet is the columnar on-disk interchange format the wider ecosystem
/// (`DuckDB`, Spark, pandas/`Polars`, the `InfluxDB`-3 FDAP stack) reads natively.
/// Like [`write_ipc_stream`], the batches must share the schema of the first; the
/// self-describing Arrow schema — including DSP's [`META_TIME_UNIT`] and
/// physical-encoding metadata — is embedded in the Parquet file (Parquet preserves
/// Arrow schema metadata), so [`read_parquet`] reconstructs the batches, and DSP's
/// columns from them, with no out-of-band information. The writer is configured
/// **without compression** (the `parquet` dependency is pulled with
/// `default-features = false` so no compression-codec C libraries reach the build);
/// values stay losslessly encoded — the typed-numeric path's exact `Decimal128`
/// and the text path are byte-faithful exactly as in the IPC export.
///
/// # Errors
///
/// Returns [`ConvertError::EmptyBatchSet`] if `batches` is empty (no schema to
/// write), or [`ConvertError::Parquet`] if the Parquet writer fails.
pub fn write_parquet(batches: &[RecordBatch]) -> Result<Vec<u8>, ConvertError> {
	let schema = batches.first().ok_or(ConvertError::EmptyBatchSet)?.schema();
	let mut buf = Vec::new();
	let mut writer = ArrowWriter::try_new(&mut buf, schema, None).map_err(|e| ConvertError::Parquet(e.to_string()))?;
	for batch in batches {
		writer.write(batch).map_err(|e| ConvertError::Parquet(e.to_string()))?;
	}
	writer.close().map_err(|e| ConvertError::Parquet(e.to_string()))?;
	Ok(buf)
}

/// Deserialize an Apache **Parquet** file (as written by [`write_parquet`]) back
/// into its [`RecordBatch`]es, in row-group order.
///
/// The inverse of [`write_parquet`]. Feed the result to
/// [`record_batch_to_columns`] / [`record_batches_to_columns`] (or
/// [`segment_from_record_batch`]) to recover DSP's columns.
///
/// # Errors
///
/// Returns [`ConvertError::Parquet`] if the bytes are not a valid Parquet file or a
/// batch fails to decode.
pub fn read_parquet(bytes: &[u8]) -> Result<Vec<RecordBatch>, ConvertError> {
	// The builder needs an owned `ChunkReader`; `bytes::Bytes` (re-exported by the
	// `parquet` crate) implements it, so copy the borrowed slice into one.
	let input = Bytes::copy_from_slice(bytes);
	let builder = ParquetRecordBatchReaderBuilder::try_new(input).map_err(|e| ConvertError::Parquet(e.to_string()))?;
	// The builder restores the embedded Arrow schema (with DSP's schema metadata);
	// re-attach it to each decoded batch so the self-describing metadata survives.
	let schema = builder.schema().clone();
	let reader = builder.build().map_err(|e| ConvertError::Parquet(e.to_string()))?;
	let mut batches = Vec::new();
	for batch in reader {
		let batch = batch.map_err(|e| ConvertError::Parquet(e.to_string()))?;
		let batch = RecordBatch::try_new(schema.clone(), batch.columns().to_vec()).map_err(|e| ConvertError::Parquet(e.to_string()))?;
		batches.push(batch);
	}
	Ok(batches)
}

/// Build a self-describing Arrow [`RecordBatch`] from a **reconstructed
/// interpolation series** — the output of DSP's flagship interpolate-on-read path,
/// not a stored segment.
///
/// Where [`columns_to_record_batch`] and [`segment_to_record_batch`] export the
/// two-column *stored* shape (`timestamp` + `value`), a reconstructed series also
/// carries per-point **provenance**, so this emits **three** non-null columns:
///
/// - `timestamp: Int64` — the output-grid epochs in `unit`.
/// - `value: Float64` — the reconstructed value. The interpolate endpoint's
///   documented wire-numeric boundary is `f64` (DSP's logical `BigDecimal` is
///   widened/narrowed at the HTTP edge, not here), so `Float64` is the exact,
///   honest column type for this surface — no false-precision claim, and no silent
///   downcast of a stored `BigDecimal` (there is none to downcast; the value is
///   already `f64` at this boundary).
/// - `kind: Utf8` — the `raw` / `interpolated` / `extrapolated` provenance token,
///   so a Parquet/Arrow consumer can distinguish an observed point from a synthetic
///   one (backlog item B-tags).
///
/// The schema metadata records the [`TimeUnit`], the value wire form
/// ([`VALUE_ENCODING_F64`]), and [`LOGICAL_EXPORT_VERSION`] (this is a logical
/// export, not an on-disk segment), so [`reconstructed_series_from_record_batch`]
/// reads it back without out-of-band information.
///
/// # Panics
///
/// Panics if the three columns do not share a row count — a caller bug, since the
/// interpolate output produces the three columns together.
#[must_use]
pub fn reconstructed_series_to_record_batch(unit: TimeUnit, timestamps: &[i64], values: &[f64], kinds: &[&str]) -> RecordBatch {
	assert_eq!(timestamps.len(), values.len(), "timestamp and value columns must share a row count");
	assert_eq!(timestamps.len(), kinds.len(), "timestamp and kind columns must share a row count");

	let ts_array = Int64Array::from_iter_values(timestamps.iter().copied());
	let val_array = Float64Array::from_iter_values(values.iter().copied());
	let kind_array = StringArray::from(kinds.to_vec());

	let mut metadata = HashMap::new();
	metadata.insert(META_TIME_UNIT.to_string(), unit.name().to_string());
	metadata.insert(META_PHYSICAL_TYPE.to_string(), PhysicalType::F64.name().to_string());
	metadata.insert(META_VALUE_ENCODING.to_string(), VALUE_ENCODING_F64.to_string());
	metadata.insert(META_FORMAT_VERSION.to_string(), LOGICAL_EXPORT_VERSION.to_string());

	let schema = Schema::new_with_metadata(
		vec![
			Field::new(TIMESTAMP_COLUMN, DataType::Int64, false),
			Field::new(VALUE_COLUMN, DataType::Float64, false),
			Field::new(SERIES_KIND_COLUMN, DataType::Utf8, false),
		],
		metadata,
	);

	RecordBatch::try_new(Arc::new(schema), vec![Arc::new(ts_array), Arc::new(val_array), Arc::new(kind_array)]).expect("timestamp, value, and kind columns share the row count")
}

/// Serialize a reconstructed interpolation series straight to **Arrow IPC stream**
/// bytes (`application/vnd.apache.arrow.stream`).
///
/// The one-call bridge the `dsp-server` interpolate endpoint uses: it holds only
/// primitive columns and receives portable bytes, so no `arrow-*` type crosses into
/// the server crate. Builds the batch with [`reconstructed_series_to_record_batch`]
/// and writes it with [`write_ipc_stream`].
///
/// # Errors
///
/// Propagates any [`ConvertError`] from [`write_ipc_stream`] (an Arrow IPC failure).
pub fn reconstructed_series_to_ipc_bytes(unit: TimeUnit, timestamps: &[i64], values: &[f64], kinds: &[&str]) -> Result<Vec<u8>, ConvertError> {
	write_ipc_stream(&[reconstructed_series_to_record_batch(unit, timestamps, values, kinds)])
}

/// Serialize a reconstructed interpolation series straight to **Apache Parquet**
/// file bytes (`application/vnd.apache.parquet`).
///
/// The Parquet counterpart of [`reconstructed_series_to_ipc_bytes`]. Builds the
/// batch with [`reconstructed_series_to_record_batch`] and writes it with
/// [`write_parquet`]; the self-describing Arrow schema (time unit, value encoding)
/// is embedded in the file.
///
/// # Errors
///
/// Propagates any [`ConvertError`] from [`write_parquet`] (a Parquet failure).
pub fn reconstructed_series_to_parquet_bytes(unit: TimeUnit, timestamps: &[i64], values: &[f64], kinds: &[&str]) -> Result<Vec<u8>, ConvertError> {
	write_parquet(&[reconstructed_series_to_record_batch(unit, timestamps, values, kinds)])
}

/// Read a reconstructed-series batch (as built by
/// [`reconstructed_series_to_record_batch`]) back into its columns:
/// `(TimeUnit, timestamps, values, kinds)`.
///
/// The inverse used to validate the round trip. Recovers the [`TimeUnit`] from the
/// schema metadata and the three columns by name and Arrow type.
///
/// # Errors
///
/// Returns a [`ConvertError`] if the time-unit metadata is missing/unrecognized or
/// a column is absent or has the wrong Arrow type.
pub fn reconstructed_series_from_record_batch(batch: &RecordBatch) -> Result<ReconstructedSeries, ConvertError> {
	let unit = unit_from_metadata(batch)?;

	let ts_col = batch.column_by_name(TIMESTAMP_COLUMN).ok_or(ConvertError::MissingColumn(TIMESTAMP_COLUMN))?;
	let ts_array = ts_col.as_any().downcast_ref::<Int64Array>().ok_or(ConvertError::WrongType { column: TIMESTAMP_COLUMN, expected: "Int64" })?;
	let timestamps: Vec<i64> = ts_array.values().to_vec();

	let val_col = batch.column_by_name(VALUE_COLUMN).ok_or(ConvertError::MissingColumn(VALUE_COLUMN))?;
	let val_array = val_col.as_any().downcast_ref::<Float64Array>().ok_or(ConvertError::WrongType { column: VALUE_COLUMN, expected: "Float64" })?;
	let values: Vec<f64> = val_array.values().to_vec();

	let kind_col = batch.column_by_name(SERIES_KIND_COLUMN).ok_or(ConvertError::MissingColumn(SERIES_KIND_COLUMN))?;
	let kind_array = kind_col.as_any().downcast_ref::<StringArray>().ok_or(ConvertError::WrongType { column: SERIES_KIND_COLUMN, expected: "Utf8" })?;
	let kinds: Vec<String> = kind_array.iter().map(|cell| cell.unwrap_or_default().to_string()).collect();

	Ok((unit, timestamps, values, kinds))
}

/// Build a self-describing Arrow [`RecordBatch`] for a **reduction table** — the
/// output of DSP's downsample/aggregation path.
///
/// A downsample reduces a series into grid-aligned buckets, reporting a per-bucket
/// sample count and one value per requested reduction (min/max/avg/sum/first/last).
/// This is the natural columnar shape for that: a non-null `timestamp: Int64`
/// (bucket start), a non-null `count: Int64` (samples in the bucket), then one
/// non-null `Float64` column per named reduction, **in the given order**.
///
/// As with [`reconstructed_series_to_record_batch`], the reduction values are `f64`
/// because that is the downsample endpoint's documented wire-numeric boundary (the
/// reductions run in `BigDecimal` and are narrowed only at the HTTP edge). The
/// schema metadata records the [`TimeUnit`], the value wire form
/// ([`VALUE_ENCODING_F64`]), and [`LOGICAL_EXPORT_VERSION`].
///
/// # Panics
///
/// Panics if any column's length differs from the timestamp column's, or if
/// `agg_names` and `agg_columns` differ in length — a caller bug, since the
/// downsample response produces the columns together.
#[must_use]
pub fn reduction_table_to_record_batch(unit: TimeUnit, timestamps: &[i64], counts: &[i64], agg_names: &[&str], agg_columns: &[Vec<f64>]) -> RecordBatch {
	assert_eq!(timestamps.len(), counts.len(), "timestamp and count columns must share a row count");
	assert_eq!(agg_names.len(), agg_columns.len(), "each reduction column must have a name");

	let mut fields = vec![Field::new(TIMESTAMP_COLUMN, DataType::Int64, false), Field::new(REDUCTION_COUNT_COLUMN, DataType::Int64, false)];
	let mut arrays: Vec<ArrayRef> = vec![Arc::new(Int64Array::from_iter_values(timestamps.iter().copied())), Arc::new(Int64Array::from_iter_values(counts.iter().copied()))];
	for (name, column) in agg_names.iter().zip(agg_columns) {
		assert_eq!(column.len(), timestamps.len(), "each reduction column must share the row count");
		fields.push(Field::new(*name, DataType::Float64, false));
		arrays.push(Arc::new(Float64Array::from_iter_values(column.iter().copied())));
	}

	let mut metadata = HashMap::new();
	metadata.insert(META_TIME_UNIT.to_string(), unit.name().to_string());
	metadata.insert(META_PHYSICAL_TYPE.to_string(), PhysicalType::F64.name().to_string());
	metadata.insert(META_VALUE_ENCODING.to_string(), VALUE_ENCODING_F64.to_string());
	metadata.insert(META_FORMAT_VERSION.to_string(), LOGICAL_EXPORT_VERSION.to_string());

	let schema = Schema::new_with_metadata(fields, metadata);
	RecordBatch::try_new(Arc::new(schema), arrays).expect("all columns share the row count")
}

/// Serialize a reduction table straight to **Arrow IPC stream** bytes
/// (`application/vnd.apache.arrow.stream`).
///
/// The one-call bridge the `dsp-server` downsample endpoint uses, mirroring
/// [`reconstructed_series_to_ipc_bytes`]: primitive columns in, portable bytes out,
/// no `arrow-*` type crossing into the server crate.
///
/// # Errors
///
/// Propagates any [`ConvertError`] from [`write_ipc_stream`].
pub fn reduction_table_to_ipc_bytes(unit: TimeUnit, timestamps: &[i64], counts: &[i64], agg_names: &[&str], agg_columns: &[Vec<f64>]) -> Result<Vec<u8>, ConvertError> {
	write_ipc_stream(&[reduction_table_to_record_batch(unit, timestamps, counts, agg_names, agg_columns)])
}

/// Serialize a reduction table straight to **Apache Parquet** file bytes
/// (`application/vnd.apache.parquet`).
///
/// The Parquet counterpart of [`reduction_table_to_ipc_bytes`].
///
/// # Errors
///
/// Propagates any [`ConvertError`] from [`write_parquet`].
pub fn reduction_table_to_parquet_bytes(unit: TimeUnit, timestamps: &[i64], counts: &[i64], agg_names: &[&str], agg_columns: &[Vec<f64>]) -> Result<Vec<u8>, ConvertError> {
	write_parquet(&[reduction_table_to_record_batch(unit, timestamps, counts, agg_names, agg_columns)])
}

/// Read a reduction-table batch (as built by [`reduction_table_to_record_batch`])
/// back into its columns: `(TimeUnit, timestamps, counts, named reductions)`.
///
/// The inverse used to validate the round trip. The reduction columns are every
/// `Float64` column other than `timestamp`/`count`, in schema order.
///
/// # Errors
///
/// Returns a [`ConvertError`] if the time-unit metadata is missing/unrecognized or
/// the `timestamp`/`count` columns are absent or have the wrong Arrow type.
pub fn reduction_table_from_record_batch(batch: &RecordBatch) -> Result<ReductionTable, ConvertError> {
	let unit = unit_from_metadata(batch)?;

	let ts_col = batch.column_by_name(TIMESTAMP_COLUMN).ok_or(ConvertError::MissingColumn(TIMESTAMP_COLUMN))?;
	let timestamps = ts_col.as_any().downcast_ref::<Int64Array>().ok_or(ConvertError::WrongType { column: TIMESTAMP_COLUMN, expected: "Int64" })?.values().to_vec();

	let count_col = batch.column_by_name(REDUCTION_COUNT_COLUMN).ok_or(ConvertError::MissingColumn(REDUCTION_COUNT_COLUMN))?;
	let counts = count_col.as_any().downcast_ref::<Int64Array>().ok_or(ConvertError::WrongType { column: REDUCTION_COUNT_COLUMN, expected: "Int64" })?.values().to_vec();

	let mut aggregations = Vec::new();
	for (idx, field) in batch.schema_ref().fields().iter().enumerate() {
		if field.name() == TIMESTAMP_COLUMN || field.name() == REDUCTION_COUNT_COLUMN {
			continue;
		}
		let column = batch.column(idx).as_any().downcast_ref::<Float64Array>().ok_or(ConvertError::WrongType { column: "reduction", expected: "Float64" })?;
		aggregations.push((field.name().clone(), column.values().to_vec()));
	}

	Ok((unit, timestamps, counts, aggregations))
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

	#[test]
	fn scaled_i64_segment_uses_arrow_decimal128_exactly() {
		// Two-decimal-place values, sealed under a declared ScaledI64{scale:2}.
		let timestamps = vec![1_i64, 2, 3, 4];
		let values = col(&["12.34", "0.05", "-7.50", "1000.00"]);
		let seg = AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, bd("0"), TimeUnit::Seconds).seal(&timestamps, &values).expect("seals");
		assert_eq!(seg.physical_type(), PhysicalType::ScaledI64 { scale: 2 });

		let batch = segment_to_record_batch_typed(&seg);
		// The value column is an exact Arrow Decimal128 at scale 2.
		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Decimal128(DECIMAL128_MAX_PRECISION, 2));
		assert_eq!(batch.schema_ref().metadata().get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_DECIMAL128));

		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		// Values reconstruct exactly — no float rounding.
		let got: Vec<BigDecimal> = vs.into_iter().map(|o| o.expect("present")).collect();
		for (g, e) in got.iter().zip(values.iter()) {
			assert_eq!(g, e, "decimal128 round trip is exact");
		}
	}

	#[test]
	fn scaled_i128_segment_round_trips_through_decimal128() {
		// A value beyond i64 once shifted, exact at scale 2 → ScaledI128.
		let big = "92233720368547758080.00"; // ~ (i64::MAX) * 10, two decimals
		let timestamps = vec![1_i64, 2];
		let values = col(&[big, "0.25"]);
		let seg = AspectSchema::new(PhysicalType::ScaledI128 { scale: 2 }, bd("0"), TimeUnit::Micros).seal(&timestamps, &values).expect("seals");
		assert_eq!(seg.physical_type(), PhysicalType::ScaledI128 { scale: 2 });

		let batch = segment_to_record_batch_typed(&seg);
		assert!(matches!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), DataType::Decimal128(_, 2)));
		let back = segment_from_record_batch(&batch, &bd("0")).expect("rebuilds");
		assert!(back.is_exact());
		assert_eq!(back.decode_values(), values);
	}

	#[test]
	fn decimal128_path_preserves_nulls() {
		let timestamps = vec![1_i64, 2, 3];
		let values = ncol(&[Some("1.25"), None, Some("9.99")]);
		let seg = AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, bd("0"), TimeUnit::Seconds).seal_nullable(&timestamps, &values).expect("seals");

		let batch = segment_to_record_batch_typed(&seg);
		let val_array = batch.column_by_name(VALUE_COLUMN).unwrap().as_any().downcast_ref::<Decimal128Array>().unwrap();
		assert_eq!(val_array.null_count(), 1);
		assert!(val_array.is_null(1));
		let (_ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(vs, values);
	}

	#[test]
	fn paged_segment_exports_one_batch_per_page_and_round_trips() {
		// 10 rows in pages of 4 ⇒ 3 pages (4 + 4 + 2).
		let timestamps: Vec<i64> = (0..10).map(|i| 1_000 + i * 10).collect();
		let values: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let paged = PagedSegment::build(&timestamps, &values, TimeUnit::Micros, &bd("0"), 4).expect("builds");
		assert_eq!(paged.page_count(), 3);

		let batches = paged_segment_to_record_batches(&paged);
		assert_eq!(batches.len(), 3);
		assert_eq!(batches[0].num_rows(), 4);
		assert_eq!(batches[2].num_rows(), 2);
		// Each batch carries the shared time unit in its metadata.
		assert_eq!(batches[1].schema_ref().metadata().get(META_TIME_UNIT).map(String::as_str), Some("micros"));

		// Concatenating the page batches recovers the full columns in order.
		let (ts, vs) = record_batches_to_columns(&batches).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values.iter().cloned().map(Some).collect::<Vec<_>>());

		// Rebuilding with the original page height reproduces the paged segment.
		let rebuilt = paged_segment_from_record_batches(&batches, &bd("0"), 4).expect("rebuilds");
		assert_eq!(rebuilt.page_count(), 3);
		assert_eq!(rebuilt.decode_nullable(), paged.decode_nullable());
	}

	#[test]
	fn paged_segment_collapses_to_a_single_batch() {
		let timestamps: Vec<i64> = (0..7).collect();
		let values = ncol(&[Some("1.0"), None, Some("3.0"), Some("4.0"), None, Some("6.0"), Some("7.0")]);
		let paged = PagedSegment::build_nullable(&timestamps, &values, TimeUnit::Seconds, &bd("0"), 3).expect("builds");
		assert_eq!(paged.page_count(), 3);

		let batch = paged_segment_to_record_batch(&paged);
		assert_eq!(batch.num_rows(), 7);
		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn empty_paged_segment_yields_no_batches() {
		let paged = PagedSegment::build(&[], &[], TimeUnit::Seconds, &bd("0"), 4).expect("builds");
		assert_eq!(paged.page_count(), 0);
		assert!(paged_segment_to_record_batches(&paged).is_empty());
		// Rebuilding from no batches cannot recover the unit, so it is an error.
		assert_eq!(paged_segment_from_record_batches(&[], &bd("0"), 4), Err(ConvertError::MissingMetadata(META_TIME_UNIT)));
	}

	#[test]
	fn logical_columns_round_trip_through_arrow() {
		// The shape a segment-store time-range read returns: a dense timestamp
		// column and a nullable value column that may have crossed several segments.
		let timestamps = vec![100_i64, 110, 120, 130, 140];
		let values = ncol(&[Some("1.5"), None, Some("3.5"), Some("4.0"), None]);
		let batch = columns_to_record_batch(TimeUnit::Millis, &timestamps, &values);

		assert_eq!(batch.num_columns(), 2);
		assert_eq!(batch.num_rows(), 5);
		// Lossless text form, with the two gaps as Arrow validity bits.
		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Utf8);
		let val_array = batch.column_by_name(VALUE_COLUMN).unwrap().as_any().downcast_ref::<StringArray>().unwrap();
		assert_eq!(val_array.null_count(), 2);

		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn logical_columns_metadata_marks_the_logical_export() {
		let batch = columns_to_record_batch(TimeUnit::Nanos, &[1_i64], &[Some(bd("2.5"))]);
		let md = batch.schema_ref().metadata();
		assert_eq!(md.get(META_TIME_UNIT).map(String::as_str), Some("nanos"));
		assert_eq!(md.get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_TEXT));
		// Physical type is the lossless logical text form, version is the sentinel.
		assert_eq!(md.get(META_PHYSICAL_TYPE).map(String::as_str), Some(PhysicalType::BigDecimalText.name()));
		assert_eq!(md.get(META_FORMAT_VERSION).map(String::as_str), Some(&LOGICAL_EXPORT_VERSION.to_string()[..]));
	}

	#[test]
	fn logical_columns_high_precision_is_lossless() {
		// Crossing into Arrow as text keeps every digit, even past f64/i128 range.
		let huge = "123456789012345678901234567890.123456789012345678901234567890";
		let timestamps = vec![1_i64, 2];
		let values = vec![Some(bd(huge)), Some(bd("-0.000000000000000000000000000001"))];
		let batch = columns_to_record_batch(TimeUnit::Nanos, &timestamps, &values);
		let (_ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(vs, values);
	}

	#[test]
	fn logical_columns_empty_is_a_zero_row_batch() {
		let batch = columns_to_record_batch(TimeUnit::Seconds, &[], &[]);
		assert_eq!(batch.num_rows(), 0);
		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert!(ts.is_empty() && vs.is_empty());
	}

	#[test]
	#[should_panic(expected = "share a row count")]
	fn logical_columns_reject_mismatched_lengths() {
		let _ = columns_to_record_batch(TimeUnit::Seconds, &[1_i64, 2], &[Some(bd("1.0"))]);
	}

	#[test]
	fn typed_logical_columns_emit_float64_and_round_trip() {
		let timestamps = vec![1_i64, 2, 3, 4];
		let values = ncol(&[Some("0.5"), None, Some("2.25"), Some("-8")]);
		let batch = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, &timestamps, &values);

		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Float64);
		assert_eq!(batch.schema_ref().metadata().get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_F64));
		// Physical type recorded is the declared one, version is the logical sentinel.
		assert_eq!(batch.schema_ref().metadata().get(META_FORMAT_VERSION).map(String::as_str), Some(&LOGICAL_EXPORT_VERSION.to_string()[..]));

		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn typed_logical_columns_emit_exact_decimal128() {
		let timestamps = vec![1_i64, 2, 3];
		let values = ncol(&[Some("12.34"), None, Some("-7.50")]);
		let batch = columns_to_record_batch_typed(TimeUnit::Millis, PhysicalType::ScaledI64 { scale: 2 }, &timestamps, &values);

		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Decimal128(DECIMAL128_MAX_PRECISION, 2));
		let (_ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		// Exact reconstruction — no float rounding.
		assert_eq!(vs, values);
	}

	#[test]
	fn typed_logical_columns_fall_back_to_text_for_variable_encoding() {
		// BigDecimalText has no fixed-width Arrow array → the typed path stays text.
		let timestamps = vec![1_i64, 2];
		let values = vec![Some(bd("123456789012345678901234567890.123")), Some(bd("1"))];
		let batch = columns_to_record_batch_typed(TimeUnit::Nanos, PhysicalType::BigDecimalText, &timestamps, &values);

		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Utf8);
		assert_eq!(batch.schema_ref().metadata().get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_TEXT));
		let (_ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(vs, values);
	}

	#[test]
	fn typed_logical_columns_match_segment_typed_export() {
		// The column-level typed path and the segment-level typed path must agree.
		let timestamps = vec![1_i64, 2, 3, 4];
		let values = col(&["0.5", "2.25", "-0.25", "128"]);
		let seg = AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds).seal(&timestamps, &values).expect("seals");

		let from_segment = segment_to_record_batch_typed(&seg);
		let nullable: Vec<Option<BigDecimal>> = values.into_iter().map(Some).collect();
		let from_columns = columns_to_record_batch_typed(TimeUnit::Seconds, PhysicalType::F64, &timestamps, &nullable);

		// Same value column type and same decoded contents (metadata version differs:
		// a real segment version vs the logical sentinel).
		assert_eq!(from_segment.column_by_name(VALUE_COLUMN).unwrap().data_type(), from_columns.column_by_name(VALUE_COLUMN).unwrap().data_type());
		assert_eq!(record_batch_to_columns(&from_segment).unwrap(), record_batch_to_columns(&from_columns).unwrap());
	}

	#[test]
	fn ipc_stream_round_trips_a_single_batch() {
		let timestamps = vec![1_i64, 2, 3];
		let values = ncol(&[Some("1.5"), None, Some("3.5")]);
		let batch = columns_to_record_batch(TimeUnit::Millis, &timestamps, &values);

		let bytes = write_ipc_stream(std::slice::from_ref(&batch)).expect("writes");
		assert!(!bytes.is_empty());
		let back = read_ipc_stream(&bytes).expect("reads");
		assert_eq!(back.len(), 1);

		// The self-describing metadata survives the stream.
		assert_eq!(back[0].schema_ref().metadata().get(META_TIME_UNIT).map(String::as_str), Some("millis"));
		let (ts, vs) = record_batch_to_columns(&back[0]).expect("reads back columns");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn ipc_stream_round_trips_typed_and_paged_batches() {
		// A multi-page export: each page batch shares one schema, so they stream together.
		let timestamps: Vec<i64> = (0..10).map(|i| 100 + i).collect();
		let values: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let paged = PagedSegment::build(&timestamps, &values, TimeUnit::Seconds, &bd("0"), 4).expect("builds");
		let batches = paged_segment_to_record_batches(&paged);
		assert_eq!(batches.len(), 3);

		let bytes = write_ipc_stream(&batches).expect("writes");
		let back = read_ipc_stream(&bytes).expect("reads");
		assert_eq!(back.len(), 3);
		let (ts, vs) = record_batches_to_columns(&back).expect("reads back columns");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values.into_iter().map(Some).collect::<Vec<_>>());
	}

	#[test]
	fn ipc_write_rejects_an_empty_batch_set() {
		assert_eq!(write_ipc_stream(&[]), Err(ConvertError::EmptyBatchSet));
	}

	#[test]
	fn ipc_read_rejects_garbage_bytes() {
		let err = read_ipc_stream(b"not an arrow stream").expect_err("must error");
		assert!(matches!(err, ConvertError::Ipc(_)), "got: {err}");
	}

	#[test]
	fn parquet_round_trips_a_single_batch() {
		let timestamps = vec![1_i64, 2, 3];
		let values = ncol(&[Some("1.5"), None, Some("3.5")]);
		let batch = columns_to_record_batch(TimeUnit::Millis, &timestamps, &values);

		let bytes = write_parquet(std::slice::from_ref(&batch)).expect("writes");
		// Parquet files start with the "PAR1" magic.
		assert_eq!(&bytes[..4], b"PAR1");
		let back = read_parquet(&bytes).expect("reads");
		assert_eq!(back.len(), 1);

		// The self-describing Arrow metadata survives the Parquet file.
		assert_eq!(back[0].schema_ref().metadata().get(META_TIME_UNIT).map(String::as_str), Some("millis"));
		let (ts, vs) = record_batch_to_columns(&back[0]).expect("reads back columns");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values);
	}

	#[test]
	fn parquet_preserves_exact_decimal128_values() {
		// The typed exact-Decimal128 path must stay byte-faithful through Parquet.
		let timestamps = vec![1_i64, 2, 3];
		let values = ncol(&[Some("12.34"), None, Some("-7.50")]);
		let batch = columns_to_record_batch_typed(TimeUnit::Millis, PhysicalType::ScaledI64 { scale: 2 }, &timestamps, &values);

		let bytes = write_parquet(std::slice::from_ref(&batch)).expect("writes");
		let back = read_parquet(&bytes).expect("reads");
		assert_eq!(back[0].column_by_name(VALUE_COLUMN).unwrap().data_type(), &DataType::Decimal128(DECIMAL128_MAX_PRECISION, 2));
		let (_ts, vs) = record_batch_to_columns(&back[0]).expect("reads back");
		// Exact reconstruction — no float rounding.
		assert_eq!(vs, values);
	}

	#[test]
	fn parquet_round_trips_paged_batches_into_columns() {
		// A multi-page export writes several row groups into one Parquet file.
		let timestamps: Vec<i64> = (0..10).map(|i| 100 + i).collect();
		let values: Vec<BigDecimal> = (0..10).map(BigDecimal::from).collect();
		let paged = PagedSegment::build(&timestamps, &values, TimeUnit::Seconds, &bd("0"), 4).expect("builds");
		let batches = paged_segment_to_record_batches(&paged);
		assert_eq!(batches.len(), 3);

		let bytes = write_parquet(&batches).expect("writes");
		let back = read_parquet(&bytes).expect("reads");
		let (ts, vs) = record_batches_to_columns(&back).expect("reads back columns");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values.into_iter().map(Some).collect::<Vec<_>>());
	}

	#[test]
	fn parquet_write_rejects_an_empty_batch_set() {
		assert_eq!(write_parquet(&[]), Err(ConvertError::EmptyBatchSet));
	}

	#[test]
	fn parquet_read_rejects_garbage_bytes() {
		let err = read_parquet(b"not a parquet file").expect_err("must error");
		assert!(matches!(err, ConvertError::Parquet(_)), "got: {err}");
	}

	#[test]
	fn reconstructed_series_batch_has_three_typed_columns() {
		let timestamps = [0_i64, 60, 120];
		let values = [0.0_f64, 30.0, 60.0];
		let kinds = ["raw", "interpolated", "raw"];
		let batch = reconstructed_series_to_record_batch(TimeUnit::Seconds, &timestamps, &values, &kinds);

		assert_eq!(batch.num_columns(), 3);
		assert_eq!(batch.num_rows(), 3);
		assert_eq!(batch.schema_ref().field(0).name(), TIMESTAMP_COLUMN);
		assert_eq!(batch.schema_ref().field(0).data_type(), &DataType::Int64);
		assert_eq!(batch.schema_ref().field(1).name(), VALUE_COLUMN);
		assert_eq!(batch.schema_ref().field(1).data_type(), &DataType::Float64);
		assert_eq!(batch.schema_ref().field(2).name(), SERIES_KIND_COLUMN);
		assert_eq!(batch.schema_ref().field(2).data_type(), &DataType::Utf8);

		let md = batch.schema_ref().metadata();
		assert_eq!(md.get(META_TIME_UNIT).map(String::as_str), Some("seconds"));
		assert_eq!(md.get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_F64));
		assert_eq!(md.get(META_FORMAT_VERSION).map(String::as_str), Some(&LOGICAL_EXPORT_VERSION.to_string()[..]));
	}

	#[test]
	fn reconstructed_series_round_trips_through_ipc() {
		let timestamps = [1_000_i64, 1_010, 1_020];
		let values = [1.5_f64, 2.25, 3.0];
		let kinds = ["raw", "interpolated", "extrapolated"];
		let bytes = reconstructed_series_to_ipc_bytes(TimeUnit::Millis, &timestamps, &values, &kinds).expect("writes ipc");

		let batches = read_ipc_stream(&bytes).expect("reads ipc");
		assert_eq!(batches.len(), 1);
		let (unit, ts, vs, ks) = reconstructed_series_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, TimeUnit::Millis);
		assert_eq!(ts, timestamps.to_vec());
		assert_eq!(vs, values.to_vec());
		assert_eq!(ks, vec!["raw", "interpolated", "extrapolated"]);
	}

	#[test]
	fn reconstructed_series_round_trips_through_parquet() {
		let timestamps = [0_i64, 1, 2, 3];
		let values = [10.0_f64, 20.0, 30.0, 40.0];
		let kinds = ["raw", "interpolated", "interpolated", "raw"];
		let bytes = reconstructed_series_to_parquet_bytes(TimeUnit::Nanos, &timestamps, &values, &kinds).expect("writes parquet");
		assert_eq!(&bytes[..4], b"PAR1");

		let batches = read_parquet(&bytes).expect("reads parquet");
		let (unit, ts, vs, ks) = reconstructed_series_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, TimeUnit::Nanos);
		assert_eq!(ts, timestamps.to_vec());
		assert_eq!(vs, values.to_vec());
		assert_eq!(ks, vec!["raw", "interpolated", "interpolated", "raw"]);
	}

	#[test]
	#[should_panic(expected = "kind columns must share a row count")]
	fn reconstructed_series_rejects_ragged_columns() {
		let _ = reconstructed_series_to_record_batch(TimeUnit::Seconds, &[0, 1], &[0.0, 1.0], &["raw"]);
	}

	#[test]
	fn reduction_table_batch_lays_out_timestamp_count_and_reductions() {
		let timestamps = [0_i64, 60];
		let counts = [3_i64, 2];
		let names = ["min", "max", "avg"];
		let columns = vec![vec![0.0_f64, 30.0], vec![20.0, 50.0], vec![10.0, 40.0]];
		let batch = reduction_table_to_record_batch(TimeUnit::Seconds, &timestamps, &counts, &names, &columns);

		assert_eq!(batch.num_columns(), 5);
		assert_eq!(batch.num_rows(), 2);
		assert_eq!(batch.schema_ref().field(0).name(), TIMESTAMP_COLUMN);
		assert_eq!(batch.schema_ref().field(1).name(), REDUCTION_COUNT_COLUMN);
		assert_eq!(batch.schema_ref().field(1).data_type(), &DataType::Int64);
		assert_eq!(batch.schema_ref().field(2).name(), "min");
		assert_eq!(batch.schema_ref().field(2).data_type(), &DataType::Float64);
		assert_eq!(batch.schema_ref().field(4).name(), "avg");
	}

	#[test]
	fn reduction_table_round_trips_through_ipc() {
		let timestamps = [0_i64, 60];
		let counts = [3_i64, 2];
		let names = ["min", "sum"];
		let columns = vec![vec![0.0_f64, 30.0], vec![30.0, 80.0]];
		let bytes = reduction_table_to_ipc_bytes(TimeUnit::Seconds, &timestamps, &counts, &names, &columns).expect("writes ipc");

		let batches = read_ipc_stream(&bytes).expect("reads ipc");
		let (unit, ts, cs, aggs) = reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, TimeUnit::Seconds);
		assert_eq!(ts, timestamps.to_vec());
		assert_eq!(cs, counts.to_vec());
		assert_eq!(aggs.len(), 2);
		assert_eq!(aggs[0], ("min".to_string(), vec![0.0, 30.0]));
		assert_eq!(aggs[1], ("sum".to_string(), vec![30.0, 80.0]));
	}

	#[test]
	fn reduction_table_round_trips_through_parquet() {
		let timestamps = [0_i64, 60];
		let counts = [1_i64, 1];
		let names = ["avg"];
		let columns = vec![vec![5.0_f64, 7.5]];
		let bytes = reduction_table_to_parquet_bytes(TimeUnit::Millis, &timestamps, &counts, &names, &columns).expect("writes parquet");
		assert_eq!(&bytes[..4], b"PAR1");

		let batches = read_parquet(&bytes).expect("reads parquet");
		let (unit, ts, cs, aggs) = reduction_table_from_record_batch(&batches[0]).expect("reads back");
		assert_eq!(unit, TimeUnit::Millis);
		assert_eq!(ts, timestamps.to_vec());
		assert_eq!(cs, counts.to_vec());
		assert_eq!(aggs, vec![("avg".to_string(), vec![5.0, 7.5])]);
	}

	#[test]
	#[should_panic(expected = "each reduction column must share the row count")]
	fn reduction_table_rejects_ragged_reduction_column() {
		let _ = reduction_table_to_record_batch(TimeUnit::Seconds, &[0, 60], &[1, 1], &["min"], &[vec![0.0]]);
	}
}
