//! # dsp-arrow-store
//!
//! The bridge that reads a **stored** DSP aspect range straight into an Apache
//! Arrow [`RecordBatch`] — joining the on-disk Storage v2 segment store
//! ([`database::SegmentStore`], roadmap **Phase 4.3**) to the Arrow interchange
//! ([`dsp_arrow`], roadmap **Phase 4.5**).
//!
//! ## Why this is its own crate
//!
//! Both halves are deliberately kept apart in the workspace:
//!
//! - `database` is a **hot-path core** crate (hard constraint #2/#3) and must stay
//!   lean — the heavy `arrow-*` dependency tree may not reach it.
//! - `dsp-arrow` depends only on `dsp-physical-type` and *never* the reverse, so it
//!   cannot reach back into `database` to read stored segments.
//!
//! So the glue lives **here**, in a leaf crate that may depend on *both* — exactly
//! the home the Phase-4.5 notes called for ("a `database`-side glue that reads a
//! stored aspect range straight into a `RecordBatch`, kept OUTSIDE the core to
//! preserve the dep boundary"). Nothing depends on this crate, so the `arrow-*`
//! tree stays out of `database`/`splimes`/`dsp-physical-type`.
//!
//! ## Vendor-neutrality (hard constraints #2 and #3)
//!
//! Arrow is an open, vendor-neutral interchange standard, not a storage backend:
//! the measurement bytes still live in DSP's own `.dspseg` segments
//! ([`database::SegmentStore`] reads them), and this crate only *re-expresses* a
//! read result in Arrow's in-memory shape. No vendor connector enters the core.
//!
//! ## What this slice covers
//!
//! Two reads, each returning a self-describing, **lossless** Arrow batch:
//!
//! - [`read_time_range_to_record_batch`] — the segment-pruned time-range read
//!   ([`SegmentStore::read_time_range`](database::SegmentStore::read_time_range)),
//!   re-expressed as a `timestamp: Int64` + nullable `value: Utf8` batch.
//! - [`read_value_range_to_record_batch`] — the value-pruned read
//!   ([`SegmentStore::read_value_range`](database::SegmentStore::read_value_range)),
//!   likewise.
//!
//! Both recover the aspect's declared [`TimeUnit`](dsp_physical_type::TimeUnit)
//! from its schema so the batch carries the right time-unit metadata, and both go
//! through [`dsp_arrow::columns_to_record_batch`], so the value column is the
//! always-exact decimal-text form — no value loses a digit crossing into Arrow
//! (hard constraint #4). A range that spans several `.dspseg` segments of
//! differing physical encodings collapses cleanly into one logical Arrow batch.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]

use anyhow::{Context, Result};
use arrow_array::RecordBatch;
use bigdecimal::BigDecimal;
use database::SegmentStore;
use dsp_physical_type::{AspectSchema, TimeUnit};

/// Read the rows of `aspect` whose timestamp falls in the inclusive `[start, end]`
/// window and return them as a single lossless Arrow [`RecordBatch`].
///
/// The read itself is the segment-pruned
/// [`SegmentStore::read_time_range`](database::SegmentStore::read_time_range) — the
/// libSQL index prunes to the overlapping `.dspseg` files before any segment byte
/// is touched, and a paged frame skips pages within the file too. The resulting
/// logical columns (which may have crossed several segments) are converted with
/// [`dsp_arrow::columns_to_record_batch`], tagged with the aspect's declared
/// [`TimeUnit`].
///
/// # Errors
///
/// Returns an error if `aspect` has no declared schema in the store (so its
/// timestamp unit is unknown), or if the underlying time-range read fails (libSQL
/// prune, filesystem read, or a corrupt `.dspseg` frame).
pub async fn read_time_range_to_record_batch(store: &SegmentStore, aspect: &str, start: i64, end: i64) -> Result<RecordBatch> {
	let unit = aspect_time_unit(store, aspect).await?;
	let (timestamps, values) = store.read_time_range(aspect, start, end).await?;
	Ok(dsp_arrow::columns_to_record_batch(unit, &timestamps, &values))
}

/// Read the rows of `aspect` in the inclusive `[start, end]` window and return them
/// as a single Arrow [`RecordBatch`] using the **typed numeric fast path** for the
/// aspect's declared physical encoding.
///
/// Identical to [`read_time_range_to_record_batch`] except for the value column's
/// Arrow form: because an aspect declares **one** physical encoding for all its
/// segments ([`AspectSchema::value`](dsp_physical_type::AspectSchema)), the whole
/// logical range shares it, so a single typed Arrow column is well-defined. An
/// [`F64`](dsp_physical_type::PhysicalType::F64) aspect yields a `Float64` column,
/// a fixed-scale [`ScaledI64`](dsp_physical_type::PhysicalType::ScaledI64) /
/// [`ScaledI128`](dsp_physical_type::PhysicalType::ScaledI128) aspect an **exact**
/// `Decimal128` column — the layout `DataFusion`/`pandas`/Flight consume directly.
/// Any other encoding falls back to the lossless decimal-text column, so the export
/// is always correct.
///
/// # Errors
///
/// As [`read_time_range_to_record_batch`]: undeclared aspect, or an underlying
/// time-range read failure.
pub async fn read_time_range_to_record_batch_typed(store: &SegmentStore, aspect: &str, start: i64, end: i64) -> Result<RecordBatch> {
	let schema = aspect_schema(store, aspect).await?;
	let (timestamps, values) = store.read_time_range(aspect, start, end).await?;
	Ok(dsp_arrow::columns_to_record_batch_typed(schema.timestamp_unit, schema.value, &timestamps, &values))
}

/// Read the present rows of `aspect` whose **value** falls in the inclusive
/// `[lo, hi]` range and return them as a single lossless Arrow [`RecordBatch`].
///
/// The read is the value-pruned
/// [`SegmentStore::read_value_range`](database::SegmentStore::read_value_range)
/// (only segments whose value span overlaps `[lo, hi]` are opened). Its present
/// values are re-expressed as the nullable Arrow value column (every row is
/// present here, so no Arrow null is emitted) with the aspect's declared
/// [`TimeUnit`].
///
/// # Errors
///
/// Returns an error if `aspect` has no declared schema in the store, or if the
/// underlying value-range read fails (libSQL read, filesystem read, or a corrupt
/// `.dspseg` frame).
pub async fn read_value_range_to_record_batch(store: &SegmentStore, aspect: &str, lo: &BigDecimal, hi: &BigDecimal) -> Result<RecordBatch> {
	let unit = aspect_time_unit(store, aspect).await?;
	let (timestamps, values) = store.read_value_range(aspect, lo, hi).await?;
	// read_value_range returns only present values; lift them into the nullable
	// shape columns_to_record_batch expects.
	let values: Vec<Option<BigDecimal>> = values.into_iter().map(Some).collect();
	Ok(dsp_arrow::columns_to_record_batch(unit, &timestamps, &values))
}

/// Read the `[start, end]` window of `aspect` and return it as **Arrow IPC stream
/// bytes**.
///
/// This is the portable wire form ready to drop into an HTTP response body, an
/// Arrow Flight payload, or a `.arrow` file on disk — the capstone of the
/// storage → Arrow path: it takes the typed time-range read
/// ([`read_time_range_to_record_batch_typed`]) and serializes the resulting batch
/// with [`dsp_arrow::write_ipc_stream`]. The stream is self-describing (the
/// time-unit and physical-encoding metadata travel in its schema), so a consumer
/// recovers the columns with [`dsp_arrow::read_ipc_stream`] +
/// [`dsp_arrow::record_batches_to_columns`] and nothing else. An empty window still
/// yields a valid single-batch stream (zero rows), not an error.
///
/// # Errors
///
/// As [`read_time_range_to_record_batch_typed`] (undeclared aspect, or a read
/// failure), plus a [`dsp_arrow::ConvertError`] if IPC serialization fails.
pub async fn read_time_range_to_ipc_bytes(store: &SegmentStore, aspect: &str, start: i64, end: i64) -> Result<Vec<u8>> {
	let batch = read_time_range_to_record_batch_typed(store, aspect, start, end).await?;
	Ok(dsp_arrow::write_ipc_stream(std::slice::from_ref(&batch))?)
}

/// Read the present rows of `aspect` whose value falls in `[lo, hi]` and return them
/// as **Arrow IPC stream bytes** (see [`read_time_range_to_ipc_bytes`]).
///
/// Serializes the lossless value-range batch
/// ([`read_value_range_to_record_batch`]).
///
/// # Errors
///
/// As [`read_value_range_to_record_batch`], plus a [`dsp_arrow::ConvertError`] if
/// IPC serialization fails.
pub async fn read_value_range_to_ipc_bytes(store: &SegmentStore, aspect: &str, lo: &BigDecimal, hi: &BigDecimal) -> Result<Vec<u8>> {
	let batch = read_value_range_to_record_batch(store, aspect, lo, hi).await?;
	Ok(dsp_arrow::write_ipc_stream(std::slice::from_ref(&batch))?)
}

/// Read the rows of `aspect` in the inclusive `[start, end]` time window and return
/// them as an Apache **Parquet** file's bytes.
///
/// The Parquet counterpart of [`read_time_range_to_ipc_bytes`]: it takes the same
/// typed time-range read ([`read_time_range_to_record_batch_typed`]) and serializes
/// the batch with [`dsp_arrow::write_parquet`]. The file is self-describing (the
/// time-unit and physical-encoding metadata travel in the embedded Arrow schema),
/// so a consumer recovers the columns with [`dsp_arrow::read_parquet`] +
/// [`dsp_arrow::record_batches_to_columns`] and nothing else — or loads it straight
/// into `DuckDB`/pandas/`Polars`. An empty window still yields a valid zero-row
/// Parquet file, not an error.
///
/// # Errors
///
/// As [`read_time_range_to_record_batch_typed`] (undeclared aspect, or a read
/// failure), plus a [`dsp_arrow::ConvertError`] if Parquet serialization fails.
pub async fn read_time_range_to_parquet_bytes(store: &SegmentStore, aspect: &str, start: i64, end: i64) -> Result<Vec<u8>> {
	let batch = read_time_range_to_record_batch_typed(store, aspect, start, end).await?;
	Ok(dsp_arrow::write_parquet(std::slice::from_ref(&batch))?)
}

/// Read the present rows of `aspect` whose value falls in `[lo, hi]` and return them
/// as an Apache **Parquet** file's bytes (see [`read_time_range_to_parquet_bytes`]).
///
/// Serializes the lossless value-range batch
/// ([`read_value_range_to_record_batch`]).
///
/// # Errors
///
/// As [`read_value_range_to_record_batch`], plus a [`dsp_arrow::ConvertError`] if
/// Parquet serialization fails.
pub async fn read_value_range_to_parquet_bytes(store: &SegmentStore, aspect: &str, lo: &BigDecimal, hi: &BigDecimal) -> Result<Vec<u8>> {
	let batch = read_value_range_to_record_batch(store, aspect, lo, hi).await?;
	Ok(dsp_arrow::write_parquet(std::slice::from_ref(&batch))?)
}

/// Resolve the declared timestamp [`TimeUnit`] for `aspect`, erroring if the aspect
/// was never declared in the store (its unit would otherwise be a guess).
async fn aspect_time_unit(store: &SegmentStore, aspect: &str) -> Result<TimeUnit> {
	Ok(aspect_schema(store, aspect).await?.timestamp_unit)
}

/// Resolve the full declared [`AspectSchema`](dsp_physical_type::AspectSchema) for
/// `aspect` (its physical encoding and timestamp unit), erroring if the aspect was
/// never declared in the store.
async fn aspect_schema(store: &SegmentStore, aspect: &str) -> Result<AspectSchema> {
	store.schema_for(aspect).await?.with_context(|| format!("aspect `{aspect}` has no declared schema in the segment store"))
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::BigDecimal;
	use database::SegmentStore;
	use dsp_arrow::{record_batch_to_columns, META_TIME_UNIT, VALUE_COLUMN};
	use dsp_physical_type::{AspectSchema, PhysicalType, TimeUnit};
	use tempfile::TempDir;

	use super::*;

	fn bd(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("test literal parses")
	}

	async fn store_with_aspect(unit: TimeUnit, physical: PhysicalType) -> (TempDir, SegmentStore) {
		let dir = TempDir::new().expect("temp dir");
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		store.declare("price", &AspectSchema::new(physical, bd("0"), unit)).await.expect("declares aspect");
		(dir, store)
	}

	#[tokio::test]
	async fn time_range_read_exports_a_lossless_batch() {
		let (_dir, store) = store_with_aspect(TimeUnit::Seconds, PhysicalType::BigDecimalText).await;
		let timestamps = vec![100_i64, 110, 120, 130, 140];
		let values = vec![bd("1.5"), bd("2.5"), bd("3.5"), bd("4.5"), bd("5.5")];
		store.seal_declared("price", &timestamps, &values).await.expect("seals");

		// A window that excludes the first and last point.
		let batch = read_time_range_to_record_batch(&store, "price", 110, 130).await.expect("exports");
		assert_eq!(batch.num_rows(), 3);
		assert_eq!(batch.schema_ref().metadata().get(META_TIME_UNIT).map(String::as_str), Some("seconds"));

		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, vec![110, 120, 130]);
		assert_eq!(vs, vec![Some(bd("2.5")), Some(bd("3.5")), Some(bd("4.5"))]);
	}

	#[tokio::test]
	async fn time_range_read_spanning_two_segments_collapses_to_one_batch() {
		let (_dir, store) = store_with_aspect(TimeUnit::Millis, PhysicalType::F64).await;
		// Two separately sealed segments → the read crosses both.
		store.seal_declared("price", &[1_i64, 2, 3], &[bd("1"), bd("2"), bd("3")]).await.expect("seals seg 1");
		store.seal_declared("price", &[4_i64, 5, 6], &[bd("4"), bd("5"), bd("6")]).await.expect("seals seg 2");

		let batch = read_time_range_to_record_batch(&store, "price", 1, 6).await.expect("exports");
		assert_eq!(batch.num_rows(), 6);
		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, vec![1, 2, 3, 4, 5, 6]);
		assert_eq!(vs, (1..=6).map(|v| Some(BigDecimal::from(v))).collect::<Vec<_>>());
	}

	#[tokio::test]
	async fn typed_time_range_read_emits_a_float64_column() {
		use arrow_array::{Array, Float64Array};
		use dsp_arrow::{META_VALUE_ENCODING, VALUE_ENCODING_F64};

		let (_dir, store) = store_with_aspect(TimeUnit::Seconds, PhysicalType::F64).await;
		let timestamps = vec![1_i64, 2, 3, 4];
		let values = vec![bd("0.5"), bd("2.25"), bd("-0.25"), bd("128")];
		store.seal_declared("price", &timestamps, &values).await.expect("seals");

		let batch = read_time_range_to_record_batch_typed(&store, "price", 1, 4).await.expect("exports");
		// A real Arrow Float64 column (downcast succeeds), not text, and the metadata
		// says so.
		assert_eq!(batch.schema_ref().metadata().get(META_VALUE_ENCODING).map(String::as_str), Some(VALUE_ENCODING_F64));
		let floats = batch.column_by_name(VALUE_COLUMN).unwrap().as_any().downcast_ref::<Float64Array>().expect("value column is Float64");
		assert_eq!(floats.values(), &[0.5, 2.25, -0.25, 128.0]);

		// And it still round-trips back to the logical columns.
		let (ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(ts, timestamps);
		assert_eq!(vs, values.into_iter().map(Some).collect::<Vec<_>>());
	}

	#[tokio::test]
	async fn value_range_read_exports_only_in_band_rows() {
		let (_dir, store) = store_with_aspect(TimeUnit::Micros, PhysicalType::BigDecimalText).await;
		let timestamps = vec![1_i64, 2, 3, 4, 5];
		let values = vec![bd("10"), bd("20"), bd("30"), bd("40"), bd("50")];
		store.seal_declared("price", &timestamps, &values).await.expect("seals");

		let batch = read_value_range_to_record_batch(&store, "price", &bd("20"), &bd("40")).await.expect("exports");
		assert_eq!(batch.num_rows(), 3);
		// Every exported row is present — no Arrow nulls in a value-range read.
		assert_eq!(batch.column_by_name(VALUE_COLUMN).unwrap().null_count(), 0);
		let (_ts, vs) = record_batch_to_columns(&batch).expect("reads back");
		assert_eq!(vs, vec![Some(bd("20")), Some(bd("30")), Some(bd("40"))]);
	}

	#[tokio::test]
	async fn undeclared_aspect_is_an_error() {
		let dir = TempDir::new().expect("temp dir");
		let store = SegmentStore::open(dir.path()).await.expect("opens store");
		// Never declare any aspect: the read cannot know the timestamp unit.
		let err = read_time_range_to_record_batch(&store, "never_declared", 0, 100).await.expect_err("must error");
		drop(store);
		assert!(err.to_string().contains("no declared schema"), "got: {err}");
	}

	#[tokio::test]
	async fn empty_window_exports_a_zero_row_batch() {
		let (_dir, store) = store_with_aspect(TimeUnit::Seconds, PhysicalType::BigDecimalText).await;
		store.seal_declared("price", &[10_i64, 20], &[bd("1"), bd("2")]).await.expect("seals");
		// A window past every stored point.
		let batch = read_time_range_to_record_batch(&store, "price", 1_000, 2_000).await.expect("exports");
		assert_eq!(batch.num_rows(), 0);
		// Still self-describing.
		assert_eq!(batch.schema_ref().metadata().get(META_TIME_UNIT).map(String::as_str), Some("seconds"));
	}

	#[tokio::test]
	async fn time_range_to_ipc_bytes_round_trips_through_the_wire_form() {
		use dsp_arrow::{read_ipc_stream, record_batches_to_columns};

		let (_dir, store) = store_with_aspect(TimeUnit::Seconds, PhysicalType::F64).await;
		let timestamps = vec![1_i64, 2, 3, 4, 5];
		let values = vec![bd("0.5"), bd("1.5"), bd("2.5"), bd("3.5"), bd("4.5")];
		store.seal_declared("price", &timestamps, &values).await.expect("seals");

		// Storage → Arrow → IPC stream bytes, the portable wire form.
		let bytes = read_time_range_to_ipc_bytes(&store, "price", 2, 4).await.expect("serializes");
		assert!(!bytes.is_empty());

		// A consumer with only dsp-arrow recovers the columns from the bytes.
		let batches = read_ipc_stream(&bytes).expect("reads stream");
		let (ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(ts, vec![2, 3, 4]);
		assert_eq!(vs, vec![Some(bd("1.5")), Some(bd("2.5")), Some(bd("3.5"))]);
	}

	#[tokio::test]
	async fn value_range_to_ipc_bytes_round_trips() {
		use dsp_arrow::{read_ipc_stream, record_batches_to_columns};

		let (_dir, store) = store_with_aspect(TimeUnit::Millis, PhysicalType::BigDecimalText).await;
		store.seal_declared("price", &[1_i64, 2, 3, 4], &[bd("10"), bd("20"), bd("30"), bd("40")]).await.expect("seals");

		let bytes = read_value_range_to_ipc_bytes(&store, "price", &bd("15"), &bd("35")).await.expect("serializes");
		let batches = read_ipc_stream(&bytes).expect("reads stream");
		let (_ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(vs, vec![Some(bd("20")), Some(bd("30"))]);
	}

	#[tokio::test]
	async fn empty_window_to_ipc_bytes_is_a_valid_zero_row_stream() {
		use dsp_arrow::read_ipc_stream;

		let (_dir, store) = store_with_aspect(TimeUnit::Seconds, PhysicalType::F64).await;
		store.seal_declared("price", &[10_i64, 20], &[bd("1"), bd("2")]).await.expect("seals");
		// A window past every stored point still serializes to a valid one-batch stream.
		let bytes = read_time_range_to_ipc_bytes(&store, "price", 1_000, 2_000).await.expect("serializes");
		let batches = read_ipc_stream(&bytes).expect("reads stream");
		assert_eq!(batches.len(), 1);
		assert_eq!(batches[0].num_rows(), 0);
	}

	#[tokio::test]
	async fn time_range_to_parquet_bytes_round_trips_through_the_file_form() {
		use dsp_arrow::{read_parquet, record_batches_to_columns};

		let (_dir, store) = store_with_aspect(TimeUnit::Seconds, PhysicalType::F64).await;
		let timestamps = vec![1_i64, 2, 3, 4, 5];
		let values = vec![bd("0.5"), bd("1.5"), bd("2.5"), bd("3.5"), bd("4.5")];
		store.seal_declared("price", &timestamps, &values).await.expect("seals");

		// Storage → Arrow → Parquet file bytes, the portable on-disk form.
		let bytes = read_time_range_to_parquet_bytes(&store, "price", 2, 4).await.expect("serializes");
		assert_eq!(&bytes[..4], b"PAR1");

		// A consumer with only dsp-arrow recovers the columns from the file.
		let batches = read_parquet(&bytes).expect("reads parquet");
		let (ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(ts, vec![2, 3, 4]);
		assert_eq!(vs, vec![Some(bd("1.5")), Some(bd("2.5")), Some(bd("3.5"))]);
	}

	#[tokio::test]
	async fn value_range_to_parquet_bytes_round_trips() {
		use dsp_arrow::{read_parquet, record_batches_to_columns};

		let (_dir, store) = store_with_aspect(TimeUnit::Millis, PhysicalType::BigDecimalText).await;
		store.seal_declared("price", &[1_i64, 2, 3, 4], &[bd("10"), bd("20"), bd("30"), bd("40")]).await.expect("seals");

		let bytes = read_value_range_to_parquet_bytes(&store, "price", &bd("15"), &bd("35")).await.expect("serializes");
		let batches = read_parquet(&bytes).expect("reads parquet");
		let (_ts, vs) = record_batches_to_columns(&batches).expect("reads back columns");
		assert_eq!(vs, vec![Some(bd("20")), Some(bd("30"))]);
	}

	#[tokio::test]
	async fn empty_window_to_parquet_bytes_is_a_valid_zero_row_file() {
		use dsp_arrow::read_parquet;

		let (_dir, store) = store_with_aspect(TimeUnit::Seconds, PhysicalType::F64).await;
		store.seal_declared("price", &[10_i64, 20], &[bd("1"), bd("2")]).await.expect("seals");
		let bytes = read_time_range_to_parquet_bytes(&store, "price", 1_000, 2_000).await.expect("serializes");
		let batches = read_parquet(&bytes).expect("reads parquet");
		let rows: usize = batches.iter().map(arrow_array::RecordBatch::num_rows).sum();
		assert_eq!(rows, 0);
	}
}
