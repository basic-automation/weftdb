//! The report as an Apache **Parquet** results table — the roadmap's `reports/parquet/`
//! surface beside the JSON and HTML ones.
//!
//! A JSON report is one nested document per run. To compare many runs (a regression history,
//! a matrix across machines) a flat table is easier: one row per [`BenchResult`], with the
//! run's metadata repeated on every row, so a directory of these files reads as one dataset in
//! `DuckDB`, Polars, pandas or a Grafana data source (`SELECT workload, p95_ns FROM
//! 'reports/parquet/*.parquet'`). Optional fields (accuracy, storage, the GPU) are nullable
//! columns, never sentinels. The JSON report stays the complete record; this table carries
//! the fields a comparison needs, not the bootstrap intervals or the storage advisories.

use std::{fs, io, path::Path, sync::Arc};

use arrow_array::{ArrayRef, BooleanArray, Float64Array, RecordBatch, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, Schema};

use crate::{report::BenchReport, schema::BenchResult};

/// One nullable-or-not column: its name, type and values.
type Column = (&'static str, DataType, bool, ArrayRef);

/// A `usize` count as the table's `u64`.
fn n(value: usize) -> u64 {
	u64::try_from(value).unwrap_or(u64::MAX)
}

/// The results table: one row per result, columns as documented on the module.
///
/// # Errors
///
/// Returns an error if the report has no results (a Parquet file needs a row group to carry
/// its schema) or the batch cannot be assembled.
pub fn to_record_batch(report: &BenchReport) -> anyhow::Result<RecordBatch> {
	anyhow::ensure!(!report.results.is_empty(), "a report with no results has no rows to write");
	let rows: &[BenchResult] = &report.results;
	let meta = &report.metadata;
	let engine = meta.engine.as_ref();
	let repeat = |value: &str| -> ArrayRef { Arc::new(StringArray::from(vec![value; rows.len()])) };
	let repeat_opt = |value: Option<&str>| -> ArrayRef { Arc::new(StringArray::from(vec![value; rows.len()])) };
	let repeat_u64 = |value: Option<u64>| -> ArrayRef { Arc::new(UInt64Array::from(vec![value; rows.len()])) };
	let text = |f: &dyn Fn(&BenchResult) -> String| -> ArrayRef { Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>())) };
	let text_opt = |f: &dyn Fn(&BenchResult) -> Option<String>| -> ArrayRef { Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>())) };
	let int = |f: &dyn Fn(&BenchResult) -> u64| -> ArrayRef { Arc::new(UInt64Array::from(rows.iter().map(f).collect::<Vec<_>>())) };
	let int_opt = |f: &dyn Fn(&BenchResult) -> Option<u64>| -> ArrayRef { Arc::new(UInt64Array::from(rows.iter().map(f).collect::<Vec<_>>())) };
	let float = |f: &dyn Fn(&BenchResult) -> f64| -> ArrayRef { Arc::new(Float64Array::from(rows.iter().map(f).collect::<Vec<_>>())) };
	let float_opt = |f: &dyn Fn(&BenchResult) -> Option<f64>| -> ArrayRef { Arc::new(Float64Array::from(rows.iter().map(f).collect::<Vec<_>>())) };
	let flag = |f: &dyn Fn(&BenchResult) -> bool| -> ArrayRef { Arc::new(BooleanArray::from(rows.iter().map(f).collect::<Vec<_>>())) };
	let shape = |r: &BenchResult| r.dataset.signal_shape.and_then(|s| serde_json::to_value(s).ok()).and_then(|v| v.as_str().map(str::to_string));

	let columns: Vec<Column> = vec![("generated_at", DataType::Utf8, false, repeat(&meta.generated_at)), ("weft_bench_version", DataType::Utf8, false, repeat(&meta.weft_bench_version)), ("report_schema_version", DataType::UInt64, false, repeat_u64(Some(u64::from(report.schema_version)))), ("os", DataType::Utf8, false, repeat(&meta.os)), ("arch", DataType::Utf8, false, repeat(&meta.arch)), ("cpu_model", DataType::Utf8, true, repeat_opt(meta.cpu_model.as_deref())), ("cpu_cores_logical", DataType::UInt64, true, repeat_u64(meta.cpu_cores_logical.map(n))), ("total_memory_bytes", DataType::UInt64, true, repeat_u64(meta.total_memory_bytes)), ("work_disk_kind", DataType::Utf8, true, repeat_opt(meta.work_disk_kind.as_deref())), ("gpu", DataType::Utf8, true, repeat_opt(engine.and_then(|e| e.gpu.as_deref()))), ("gpu_driver", DataType::Utf8, true, repeat_opt(engine.and_then(|e| e.gpu_driver.as_deref()))), ("profile", DataType::Utf8, false, text(&|r| r.profile.clone())), ("adapter", DataType::Utf8, false, text(&|r| r.adapter.clone())), ("workload", DataType::Utf8, false, text(&|r| r.workload.clone())), ("reps", DataType::UInt64, false, int(&|r| n(r.reps))), ("input_points", DataType::UInt64, false, int(&|r| n(r.dataset.input_points))), ("output_points", DataType::UInt64, false, int(&|r| n(r.dataset.output_points))), ("irregular", DataType::Boolean, false, flag(&|r| r.dataset.irregular)), ("missingness_fraction", DataType::Float64, false, float(&|r| r.dataset.missingness_fraction)), ("seed", DataType::UInt64, false, int(&|r| r.dataset.seed)), ("signal_shape", DataType::Utf8, true, text_opt(&shape)), ("min_ns", DataType::UInt64, false, int(&|r| r.latency.min_ns)), ("p50_ns", DataType::UInt64, false, int(&|r| r.latency.p50_ns)), ("p95_ns", DataType::UInt64, false, int(&|r| r.latency.p95_ns)), ("p99_ns", DataType::UInt64, false, int(&|r| r.latency.p99_ns)), ("max_ns", DataType::UInt64, false, int(&|r| r.latency.max_ns)), ("mean_ns", DataType::UInt64, false, int(&|r| r.latency.mean_ns)), ("stddev_ns", DataType::UInt64, false, int(&|r| r.latency.stddev_ns)), ("first_rep_ns", DataType::UInt64, true, int_opt(&|r| r.cold_warm.as_ref().map(|c| c.first_rep_ns))), ("warm_p50_ns", DataType::UInt64, true, int_opt(&|r| r.cold_warm.as_ref().map(|c| c.warm.p50_ns))), ("throughput_points_per_sec", DataType::Float64, false, float(&|r| r.throughput_points_per_sec)), ("dataset_generation_ns", DataType::UInt64, false, int(&|r| r.timing.dataset_generation_ns)), ("measured_ns", DataType::UInt64, false, int(&|r| r.timing.measured_ns)), ("end_to_end_ns", DataType::UInt64, false, int(&|r| r.timing.end_to_end_ns)), ("correctness_passed", DataType::Boolean, false, flag(&|r| r.correctness.passed())), ("expected_output_points", DataType::UInt64, false, int(&|r| n(r.correctness.expected_output_points))), ("actual_output_points", DataType::UInt64, false, int(&|r| n(r.correctness.actual_output_points))), ("accuracy_rmse", DataType::Float64, true, float_opt(&|r| r.accuracy.map(|a| a.rmse))), ("accuracy_mae", DataType::Float64, true, float_opt(&|r| r.accuracy.map(|a| a.mae))), ("accuracy_max_abs_error", DataType::Float64, true, float_opt(&|r| r.accuracy.map(|a| a.max_abs_error))), ("accuracy_bias", DataType::Float64, true, float_opt(&|r| r.accuracy.map(|a| a.bias))), ("storage_total_bytes_per_point", DataType::Float64, true, float_opt(&|r| r.storage.as_ref().map(|s| s.total_bytes_per_point))), ("storage_value_codec", DataType::Utf8, true, text_opt(&|r| r.storage.as_ref().map(|s| s.value_codec.clone())))];
	let schema = Arc::new(Schema::new(columns.iter().map(|(name, ty, nullable, _)| Field::new(*name, ty.clone(), *nullable)).collect::<Vec<_>>()));
	RecordBatch::try_new(schema, columns.into_iter().map(|(.., values)| values).collect()).map_err(|e| anyhow::anyhow!("assembling the results table: {e}"))
}

/// The results table as Parquet file bytes (uncompressed, as `weft-arrow` writes them).
///
/// # Errors
///
/// As [`to_record_batch`], or if the Parquet writer fails.
pub fn to_parquet(report: &BenchReport) -> anyhow::Result<Vec<u8>> {
	let batch = to_record_batch(report)?;
	weft_arrow::write_parquet(&[batch]).map_err(|e| anyhow::anyhow!("writing the results table: {e}"))
}

/// Write the results table to `path` as Parquet, creating missing parent directories.
///
/// # Errors
///
/// As [`to_parquet`], or if a directory or the file cannot be written.
pub fn write_parquet(report: &BenchReport, path: impl AsRef<Path>) -> anyhow::Result<()> {
	let path = path.as_ref();
	if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
		fs::create_dir_all(parent)?;
	}
	fs::write(path, to_parquet(report)?).map_err(|e: io::Error| anyhow::anyhow!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
	use arrow_array::Array;

	use super::*;
	use crate::{
		downsample::{run_downsample, DownsampleParams, DownsampleProfile}, gap_fill::{run_gap_fill, GapFillParams, GapFillProfile}, report::RunMetadata
	};

	fn report() -> BenchReport {
		let downsample = run_downsample(&DownsampleProfile::new("pq-ds", DownsampleParams { point_count: 3_600, ..DownsampleParams::default() }), 2).expect("runs");
		let gap_fill = run_gap_fill(&GapFillProfile::new("pq-gf", GapFillParams { point_count: 4 * 3_600, ..GapFillParams::default() }), 2).expect("runs");
		BenchReport::with_results(RunMetadata::capture("2026-10-09T00:00:00Z".to_string()), vec![downsample, gap_fill])
	}

	#[test]
	fn one_row_per_result_survives_a_parquet_round_trip() {
		let report = report();
		let batch = to_record_batch(&report).expect("assembles");
		assert_eq!(batch.num_rows(), 2);
		let bytes = to_parquet(&report).expect("writes");
		let read = weft_arrow::read_parquet(&bytes).expect("reads");
		assert_eq!(read.len(), 1);
		assert_eq!(read[0], batch, "the table reads back unchanged");

		let column = |name: &str| read[0].column_by_name(name).unwrap_or_else(|| panic!("column {name}")).clone();
		let workloads = column("workload");
		let workloads = workloads.as_any().downcast_ref::<StringArray>().expect("utf8");
		assert_eq!((workloads.value(0), workloads.value(1)), ("downsample", "gap_fill"));
		let p95 = column("p95_ns");
		let p95 = p95.as_any().downcast_ref::<UInt64Array>().expect("u64");
		assert_eq!(p95.value(1), report.results[1].latency.p95_ns);
		// The downsample has no accuracy, the gap fill does: a null, not a sentinel.
		let rmse = column("accuracy_rmse");
		let rmse = rmse.as_any().downcast_ref::<Float64Array>().expect("f64");
		assert!(rmse.is_null(0));
		assert_eq!(rmse.value(1), report.results[1].accuracy.expect("scored").rmse);
		let passed = column("correctness_passed");
		assert!(passed.as_any().downcast_ref::<BooleanArray>().expect("bool").value(0));
		let generated = column("generated_at");
		assert_eq!(generated.as_any().downcast_ref::<StringArray>().expect("utf8").value(1), "2026-10-09T00:00:00Z", "run metadata repeats on every row");
	}

	#[test]
	fn an_empty_report_is_refused_and_files_land_in_new_directories() {
		assert!(to_parquet(&BenchReport::new(RunMetadata::capture("t".to_string()))).is_err());
		let dir = std::env::temp_dir().join(format!("weft-bench-parquet-{}", std::process::id()));
		let path = dir.join("nested").join("run.parquet");
		write_parquet(&report(), &path).expect("writes");
		assert_eq!(weft_arrow::read_parquet(&fs::read(&path).expect("reads")).expect("parses")[0].num_rows(), 2);
		fs::remove_dir_all(&dir).ok();
	}
}
