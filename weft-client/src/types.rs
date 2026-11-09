//! What goes over the wire: request options, and the answers the client decodes.
//!
//! Two numeric conventions follow the server's own boundary. The **compute** endpoints
//! speak `f64` and RFC 3339 instants ([`ComputePoint`]). The **storage** endpoints are
//! lossless: stored timestamps are integers in the aspect's declared unit and values are
//! decimal text, which this crate reads as [`BigDecimal`] and writes from any
//! [`StoredValue`] — and `f64` is deliberately not one.

use std::{collections::BTreeMap, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Error, Result};

// ── values ────────────────────────────────────────────────────────────────────

/// A value the storage endpoints can hold without rounding: it becomes decimal text.
///
/// Implemented for [`BigDecimal`], the integer types, decimal strings, and `Option` of
/// any of them (`None` is a null row). Not for `f32`/`f64`: a float's binary expansion is
/// not the number that was typed, so it is refused here, at compile time, rather than
/// stored as something nobody wrote.
///
/// ```compile_fail
/// # async fn f(client: &weftdb_client::Client) {
/// client.write_points("price", [(1_i64, 1.25_f64)], &Default::default()).await;
/// # }
/// ```
pub trait StoredValue {
	/// The value as decimal text, `None` for a null.
	///
	/// # Errors
	///
	/// Text that is not a decimal.
	fn to_decimal_text(&self) -> Result<Option<String>>;
}

impl StoredValue for BigDecimal {
	fn to_decimal_text(&self) -> Result<Option<String>> {
		Ok(Some(self.to_plain_string()))
	}
}

/// The widest exponent a decimal string may carry. Plain notation spells every digit an
/// exponent implies, so `1e999999999` would be a gigabyte of zeros; no value the server can
/// store needs more than a few dozen digits either side of the point.
const MAX_EXPONENT: i64 = 4_096;

impl StoredValue for str {
	fn to_decimal_text(&self) -> Result<Option<String>> {
		let parsed = BigDecimal::from_str(self.trim()).map_err(|_| Error::invalid(format!("{self:?} is not a decimal")))?;
		let (_, scale) = parsed.as_bigint_and_exponent();
		if scale.unsigned_abs() > MAX_EXPONENT.unsigned_abs() {
			return Err(Error::invalid(format!("{self:?} spans more than {MAX_EXPONENT} decimal places; no stored type holds it")));
		}
		Ok(Some(parsed.to_plain_string()))
	}
}

impl StoredValue for String {
	fn to_decimal_text(&self) -> Result<Option<String>> {
		self.as_str().to_decimal_text()
	}
}

impl<T: StoredValue + ?Sized> StoredValue for &T {
	fn to_decimal_text(&self) -> Result<Option<String>> {
		(**self).to_decimal_text()
	}
}

impl<T: StoredValue> StoredValue for Option<T> {
	fn to_decimal_text(&self) -> Result<Option<String>> {
		self.as_ref().map_or_else(|| Ok(None), StoredValue::to_decimal_text)
	}
}

macro_rules! integer_values {
	($($t:ty),*) => {$(
		impl StoredValue for $t {
			fn to_decimal_text(&self) -> Result<Option<String>> {
				Ok(Some(self.to_string()))
			}
		}
	)*};
}
integer_values!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, isize, usize);

fn decimal_from_text<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Option<BigDecimal>, D::Error> {
	Option::<String>::deserialize(deserializer)?.map(|text| BigDecimal::from_str(&text).map_err(|_| serde::de::Error::custom(format!("{text:?} is not decimal text")))).transpose()
}

// ── service ───────────────────────────────────────────────────────────────────

/// One benchmark run the server holds, from `GET /bench/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct BenchRun {
	/// The run id: the SHA-256 (lowercase hex) of the manifest file's bytes.
	pub id: String,
	/// The manifest's path under the server's run directory.
	pub path: String,
	/// The manifest's schema version.
	#[serde(default)]
	pub schema_version: Option<u64>,
	/// When the run was generated (RFC 3339, as the manifest says it).
	#[serde(default)]
	pub generated_at: Option<String>,
	/// The claim the run backs, if it names one.
	#[serde(default)]
	pub claim: Option<String>,
}

#[derive(Deserialize)]
pub struct BenchRunList {
	pub runs: Vec<BenchRun>,
}

// ── compute ───────────────────────────────────────────────────────────────────

/// One input point of the compute endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ComputePoint {
	/// The instant, in UTC.
	pub timestamp: DateTime<Utc>,
	/// The sample.
	pub value: f64,
}

impl ComputePoint {
	/// A point from an instant and a sample.
	#[must_use]
	pub const fn new(timestamp: DateTime<Utc>, value: f64) -> Self {
		Self { timestamp, value }
	}
}

impl From<(DateTime<Utc>, f64)> for ComputePoint {
	fn from((timestamp, value): (DateTime<Utc>, f64)) -> Self {
		Self { timestamp, value }
	}
}

/// The reconstruction a compute request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Spline {
	/// Piecewise linear.
	Linear,
	/// Piecewise quadratic.
	Quadratic,
	/// Natural cubic (the server's default).
	#[default]
	Cubic,
	/// A polynomial of the given degree.
	Polynomial {
		/// The polynomial's degree.
		degree: u32,
	},
}

impl Spline {
	pub(crate) const fn query_token(self) -> Option<&'static str> {
		match self {
			Self::Linear => Some("linear"),
			Self::Quadratic => Some("quadratic"),
			Self::Cubic => Some("cubic"),
			Self::Polynomial { .. } => None,
		}
	}
}

/// What an answer is encoded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
	/// JSON (the typed methods).
	Json,
	/// CSV text.
	Csv,
	/// An Arrow IPC stream.
	Arrow,
	/// A Parquet file.
	Parquet,
}

impl Format {
	pub(crate) const fn name(self) -> &'static str {
		match self {
			Self::Json => "json",
			Self::Csv => "csv",
			Self::Arrow => "arrow",
			Self::Parquet => "parquet",
		}
	}
}

/// `POST /api/v1/interpolate`: reconstruct points on a regular grid.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Interpolate {
	/// The reconstruction.
	pub spline: Spline,
	/// The grid step, a resolution name: `milliseconds`, `seconds`, `minutes`, `hours`, `days`, …
	/// (the JSON endpoint takes names, not widths such as `"250ms"`).
	pub resolution: String,
	/// The grid's first instant (default: the first point's).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub start: Option<DateTime<Utc>>,
	/// The grid's last instant (default: the last point's).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub end: Option<DateTime<Utc>>,
	/// The input series.
	pub points: Vec<ComputePoint>,
}

impl Interpolate {
	/// A request over `points`: cubic, on a one-second grid.
	pub fn new(points: impl IntoIterator<Item = impl Into<ComputePoint>>) -> Self {
		Self { spline: Spline::Cubic, resolution: "seconds".into(), start: None, end: None, points: points.into_iter().map(Into::into).collect() }
	}

	/// Use `spline`.
	#[must_use]
	pub const fn spline(mut self, spline: Spline) -> Self {
		self.spline = spline;
		self
	}

	/// Use the grid step `resolution`.
	#[must_use]
	pub fn resolution(mut self, resolution: impl Into<String>) -> Self {
		self.resolution = resolution.into();
		self
	}

	/// Bound the grid to `[start, end]`.
	#[must_use]
	pub const fn window(mut self, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> Self {
		self.start = start;
		self.end = end;
		self
	}
}

/// Where an output point came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PointKind {
	/// An input point, passed through.
	Raw,
	/// Inside the input's span.
	Interpolated,
	/// Outside it.
	Extrapolated,
	/// A kind this client version does not know.
	#[serde(other)]
	Unknown,
}

/// One point of an interpolated series.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct OutputPoint {
	/// The grid instant.
	pub timestamp: DateTime<Utc>,
	/// The reconstructed value.
	pub value: f64,
	/// Raw, interpolated or extrapolated.
	pub kind: PointKind,
}

/// The answer to [`Interpolate`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct Interpolated {
	/// The spline used.
	pub spline: String,
	/// The grid step used.
	pub resolution: String,
	/// How many points went in.
	pub input_points: usize,
	/// How many came out.
	pub output_points: usize,
	/// The series.
	pub points: Vec<OutputPoint>,
}

/// The answer to `POST /api/v1/interpolate/point`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct InterpolatedPoint {
	/// The spline used.
	pub spline: String,
	/// The instant evaluated.
	pub instant: DateTime<Utc>,
	/// How many points went in.
	pub input_points: usize,
	/// The reconstructed value there.
	pub value: f64,
}

#[derive(Serialize)]
pub struct InterpolatePointBody<'a> {
	pub spline: Spline,
	pub instant: DateTime<Utc>,
	pub points: &'a [ComputePoint],
}

/// `POST /api/v1/downsample`: reduce points into epoch-aligned buckets.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Downsample {
	/// The bucket resolution, a name: `seconds`, `minutes`, `hours`, `days`, … (a count such
	/// as `"15m"` goes in [`width`](Self::width)).
	pub resolution: String,
	/// The reductions per bucket: `avg`, `min`, `max`, `sum`, `count`, `first`, `last`, ….
	pub aggregations: Vec<String>,
	/// The first bucket's instant.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub start: Option<DateTime<Utc>>,
	/// The last bucket's instant.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub end: Option<DateTime<Utc>>,
	/// A bucket width that overrides `resolution`.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub width: Option<String>,
	/// How empty buckets are filled (`null`, `previous`, `linear`, …).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub fill: Option<String>,
	/// The input series.
	pub points: Vec<ComputePoint>,
}

impl Downsample {
	/// A request reducing `points` with `aggregations` into one-second buckets.
	pub fn new(points: impl IntoIterator<Item = impl Into<ComputePoint>>, aggregations: impl IntoIterator<Item = impl Into<String>>) -> Self {
		Self { resolution: "seconds".into(), aggregations: aggregations.into_iter().map(Into::into).collect(), start: None, end: None, width: None, fill: None, points: points.into_iter().map(Into::into).collect() }
	}

	/// Use the bucket width `resolution`.
	#[must_use]
	pub fn resolution(mut self, resolution: impl Into<String>) -> Self {
		self.resolution = resolution.into();
		self
	}

	/// Buckets `width` wide, e.g. `"15m"` or `"250ms"` (overrides the resolution).
	#[must_use]
	pub fn width(mut self, width: impl Into<String>) -> Self {
		self.width = Some(width.into());
		self
	}

	/// Fill empty buckets with `fill`.
	#[must_use]
	pub fn fill(mut self, fill: impl Into<String>) -> Self {
		self.fill = Some(fill.into());
		self
	}
}

/// One bucket of a downsampled series.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct Bucket {
	/// The bucket's start.
	pub timestamp: DateTime<Utc>,
	/// How many input points fell in it.
	pub count: usize,
	/// Each requested reduction's value, by name. A reduction with no answer (an empty
	/// bucket) is absent.
	#[serde(default)]
	pub aggregations: BTreeMap<String, Option<f64>>,
}

/// The answer to [`Downsample`] and to the stored downsample.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct Downsampled {
	/// The bucket width used.
	pub resolution: String,
	/// The reductions computed.
	pub aggregations: Vec<String>,
	/// How many points went in.
	pub input_points: usize,
	/// How many buckets came out.
	pub buckets: usize,
	/// The buckets.
	pub series: Vec<Bucket>,
	/// The fill used, if one was asked for.
	#[serde(default)]
	pub fill: Option<String>,
}

/// The query of the Line Protocol compute endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct IlpOptions {
	/// The field to read from each line (required by the server).
	pub field: String,
	/// The timestamp precision: `ns` (default), `us`, `ms`, `s`.
	pub precision: Option<String>,
	/// The reconstruction (interpolate only).
	pub spline: Option<Spline>,
	/// Interpolation: the grid step, a resolution name (`seconds`, …). Downsample: the bucket
	/// width, a name or a count such as `"15m"`.
	pub resolution: Option<String>,
	/// The reductions (downsample only).
	pub aggregations: Vec<String>,
	/// The empty-bucket fill (downsample only).
	pub fill: Option<String>,
}

impl IlpOptions {
	/// Options reading `field`.
	pub fn field(field: impl Into<String>) -> Self {
		Self { field: field.into(), ..Self::default() }
	}

	/// The timestamp precision of the body.
	#[must_use]
	pub fn precision(mut self, precision: impl Into<String>) -> Self {
		self.precision = Some(precision.into());
		self
	}

	/// The reconstruction, for interpolation.
	#[must_use]
	pub const fn spline(mut self, spline: Spline) -> Self {
		self.spline = Some(spline);
		self
	}

	/// The grid step or bucket width.
	#[must_use]
	pub fn resolution(mut self, resolution: impl Into<String>) -> Self {
		self.resolution = Some(resolution.into());
		self
	}
}

// ── catalog ───────────────────────────────────────────────────────────────────

/// `POST /api/v1/storage/aspects`: an aspect's schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct AspectDeclaration {
	/// The aspect's name.
	pub name: String,
	/// The physical encoding: `f64`, `f32`, `scaled_i64`, `scaled_i128`, `decimal128` or `bigdecimal_text`.
	pub physical_type: String,
	/// The timestamp unit: `seconds`, `millis`, `micros`, `nanos`.
	pub timestamp_unit: String,
	/// The fractional digits, for the fixed-point encodings.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub scale: Option<u8>,
	/// The per-value error tolerance, as decimal text (`"0"` is exact, the default).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub value_tolerance: Option<String>,
}

impl AspectDeclaration {
	/// A declaration of `name` with `physical_type` and `timestamp_unit`.
	pub fn new(name: impl Into<String>, physical_type: impl Into<String>, timestamp_unit: impl Into<String>) -> Self {
		Self { name: name.into(), physical_type: physical_type.into(), timestamp_unit: timestamp_unit.into(), scale: None, value_tolerance: None }
	}

	/// The fixed-point scale.
	#[must_use]
	pub const fn scale(mut self, scale: u8) -> Self {
		self.scale = Some(scale);
		self
	}

	/// The value tolerance; refused unless it is a decimal.
	///
	/// # Errors
	///
	/// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a value that is not a decimal.
	pub fn value_tolerance(mut self, tolerance: impl StoredValue) -> Result<Self> {
		self.value_tolerance = tolerance.to_decimal_text()?;
		Ok(self)
	}
}

/// A declared aspect's schema, as the server reads it back.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct AspectInfo {
	/// The aspect's name.
	pub name: String,
	/// The physical encoding.
	pub physical_type: String,
	/// The fixed-point scale, for the encodings that have one.
	#[serde(default)]
	pub scale: Option<u8>,
	/// The value tolerance (decimal text).
	pub value_tolerance: String,
	/// The timestamp unit.
	pub timestamp_unit: String,
}

#[derive(Deserialize)]
pub struct AspectEnvelope {
	pub aspect: AspectInfo,
}

#[derive(Deserialize)]
pub struct AspectList {
	pub aspects: Vec<AspectInfo>,
}

/// One aspect's storage totals.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct AspectStats {
	/// The aspect.
	pub aspect: String,
	/// Its live segments.
	pub segment_count: usize,
	/// Its rows.
	pub total_rows: u64,
	/// Its null rows.
	pub total_nulls: u64,
	/// Its bytes on disk.
	pub total_bytes: u64,
	/// Bytes per stored point.
	pub bytes_per_point: f64,
}

/// The store's totals.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct StoreStats {
	/// Declared aspects.
	pub aspect_count: usize,
	/// Live segments.
	pub segment_count: usize,
	/// Rows.
	pub total_rows: u64,
	/// Null rows.
	pub total_nulls: u64,
	/// Bytes on disk.
	pub total_bytes: u64,
	/// Bytes per stored point.
	pub bytes_per_point: f64,
}

// ── ingest ────────────────────────────────────────────────────────────────────

/// How a write is sealed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriteOptions {
	/// Rows per page of the sealed segment (the server picks when absent).
	pub rows_per_page: Option<u32>,
	/// Refuse the batch if its timestamps are not non-decreasing.
	pub require_sorted: bool,
	/// Sent as `Idempotency-Key`: a retry of the same batch answers `replayed: true`
	/// instead of sealing it twice.
	pub idempotency_key: Option<String>,
	/// The Line Protocol body's timestamp precision (`write_ilp` only).
	pub precision: Option<String>,
	/// The Line Protocol measurement whose records are sealed (`write_ilp` only). The
	/// server requires it since TAG-11; the records' own tags name the series.
	pub measurement: Option<String>,
}

impl WriteOptions {
	/// Options carrying `key` as the batch's `Idempotency-Key`.
	pub fn idempotent(key: impl Into<String>) -> Self {
		Self { idempotency_key: Some(key.into()), ..Self::default() }
	}

	/// Refuse an unsorted batch.
	#[must_use]
	pub const fn require_sorted(mut self) -> Self {
		self.require_sorted = true;
		self
	}

	/// The Line Protocol body's timestamp precision (`ns`, `us`, `ms`, `s`).
	#[must_use]
	pub fn precision(mut self, precision: impl Into<String>) -> Self {
		self.precision = Some(precision.into());
		self
	}

	/// The Line Protocol measurement to seal (`write_ilp` only; required by the server).
	#[must_use]
	pub fn measurement(mut self, measurement: impl Into<String>) -> Self {
		self.measurement = Some(measurement.into());
		self
	}

	/// Seal with `rows` rows per page.
	#[must_use]
	pub const fn rows_per_page(mut self, rows: u32) -> Self {
		self.rows_per_page = Some(rows);
		self
	}
}

#[derive(Serialize)]
pub struct WireStoredPoint {
	pub timestamp: i64,
	pub value: Option<String>,
}

#[derive(Serialize)]
pub struct PointsBody {
	pub points: Vec<WireStoredPoint>,
	pub require_sorted: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub rows_per_page: Option<u32>,
}

/// What a write sealed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct WriteSummary {
	/// The aspect written.
	pub aspect: String,
	/// The (first) segment sealed.
	pub segment_id: u64,
	/// The segment format version.
	pub format_version: u16,
	/// Rows sealed.
	pub row_count: usize,
	/// Null rows among them.
	pub null_count: usize,
	/// Bytes sealed.
	pub byte_len: u64,
	/// The earliest timestamp.
	pub min_ts: Option<i64>,
	/// The latest timestamp.
	pub max_ts: Option<i64>,
	/// Whether the batch arrived sorted.
	pub time_sorted: bool,
	/// Every segment sealed, when the batch spanned more than one.
	#[serde(default)]
	pub segment_ids: Vec<u64>,
	/// `true` when this answers an earlier commit of the same `Idempotency-Key`.
	#[serde(default)]
	pub replayed: bool,
}

// ── stored reads ──────────────────────────────────────────────────────────────

/// One stored row: its timestamp and its exact value, or `None` for a null.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct StoredPoint {
	/// Epoch integer in the aspect's declared unit.
	pub timestamp: i64,
	/// The value, exactly as stored.
	#[serde(default, deserialize_with = "decimal_from_text")]
	pub value: Option<BigDecimal>,
}

/// One page of a stored read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct PointsPage {
	/// The aspect read.
	pub aspect: String,
	/// The aspect's timestamp unit.
	pub time_unit: String,
	/// Rows in the whole window, before paging.
	#[serde(default)]
	pub total: Option<usize>,
	/// Rows in this page.
	pub count: usize,
	/// Rows skipped before this page.
	pub offset: usize,
	/// The page size applied.
	#[serde(default)]
	pub limit: Option<usize>,
	/// The cursor for the next page; absent on the last.
	#[serde(default)]
	pub next_cursor: Option<String>,
	/// The rows.
	pub points: Vec<StoredPoint>,
}

/// Which page of a stored read to fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Page {
	/// At most this many rows.
	pub limit: Option<usize>,
	/// Skip this many first (ignored with a cursor).
	pub offset: Option<usize>,
	/// Continue from a previous page's `next_cursor`.
	pub cursor: Option<String>,
}

impl Page {
	/// The first `limit` rows.
	#[must_use]
	pub fn first(limit: usize) -> Self {
		Self { limit: Some(limit), ..Self::default() }
	}

	/// Up to `limit` rows after `cursor`.
	pub fn after(cursor: impl Into<String>, limit: usize) -> Self {
		Self { limit: Some(limit), offset: None, cursor: Some(cursor.into()) }
	}
}

/// A stored value at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct PointAt {
	/// The instant asked about.
	pub timestamp: i64,
	/// The present value there, if any.
	#[serde(default, deserialize_with = "decimal_from_text")]
	pub value: Option<BigDecimal>,
	/// Whether a present value was found.
	pub found: bool,
}

#[derive(Deserialize)]
pub struct PointsAt {
	pub points: Vec<PointAt>,
}

/// The query of the stored downsample.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StoredDownsample {
	/// The window's start (epoch integer in the declared unit).
	pub start: Option<i64>,
	/// The window's end.
	pub end: Option<i64>,
	/// The bucket width.
	pub resolution: Option<String>,
	/// The reductions.
	pub aggregations: Vec<String>,
	/// The empty-bucket fill.
	pub fill: Option<String>,
}

impl StoredDownsample {
	/// Reduce `[start, end]` into `resolution` buckets with `aggregations`.
	pub fn new(start: i64, end: i64, resolution: impl Into<String>, aggregations: impl IntoIterator<Item = impl Into<String>>) -> Self {
		Self { start: Some(start), end: Some(end), resolution: Some(resolution.into()), aggregations: aggregations.into_iter().map(Into::into).collect(), fill: None }
	}
}

/// Decimal text from one of the untyped maintenance answers, read exactly.
///
/// # Errors
///
/// [`ErrorKind::Decode`](crate::ErrorKind::Decode) if `text` is not decimal text.
pub fn parse_decimal(text: &str) -> Result<BigDecimal> {
	BigDecimal::from_str(text).map_err(|_| Error::new(crate::ErrorKind::Decode, format!("{text:?} is not decimal text")))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stored_values_become_exact_decimal_text() {
		assert_eq!(BigDecimal::from_str("1.250").unwrap().to_decimal_text().unwrap(), Some("1.250".into()));
		assert_eq!("  2.50 ".to_decimal_text().unwrap(), Some("2.50".into()));
		assert_eq!(42_i64.to_decimal_text().unwrap(), Some("42".into()));
		assert_eq!(u128::MAX.to_decimal_text().unwrap(), Some(u128::MAX.to_string()));
		assert_eq!(None::<i64>.to_decimal_text().unwrap(), None);
		assert_eq!(Some("3").to_decimal_text().unwrap(), Some("3".into()));
		assert_eq!(BigDecimal::from_str("1e-3").unwrap().to_decimal_text().unwrap(), Some("0.001".into()), "plain notation, which every parser on the server takes");
		assert_eq!("1e999999999".to_decimal_text().unwrap_err().kind(), crate::ErrorKind::InvalidInput, "refused before it is spelled out");
		assert_eq!("1e-5000".to_decimal_text().unwrap_err().kind(), crate::ErrorKind::InvalidInput);
		let refused = "1.2.3".to_decimal_text().unwrap_err();
		assert_eq!(refused.kind(), crate::ErrorKind::InvalidInput);
	}

	#[test]
	fn a_stored_page_reads_its_values_losslessly() {
		let page: PointsPage = serde_json::from_str(r#"{"aspect":"p","time_unit":"millis","total":3,"count":2,"offset":0,"limit":2,"next_cursor":"c1","points":[{"timestamp":1,"value":"0.1000000000000000000001"},{"timestamp":2,"value":null}]}"#).unwrap();
		assert_eq!(page.points[0].value.as_ref().unwrap().to_string(), "0.1000000000000000000001");
		assert_eq!(page.points[1].value, None);
		assert_eq!(page.next_cursor.as_deref(), Some("c1"));
		let at: PointAt = serde_json::from_str(r#"{"aspect":"p","time_unit":"millis","timestamp":5,"found":false}"#).unwrap();
		assert!(!at.found && at.value.is_none());
		assert!(serde_json::from_str::<StoredPoint>(r#"{"timestamp":1,"value":"x"}"#).is_err());
	}

	#[test]
	fn a_response_without_total_decodes_as_none() {
		let with_total: PointsPage = serde_json::from_str(r#"{"aspect":"p","time_unit":"millis","total":12,"count":2,"offset":0,"points":[{"timestamp":1,"value":"1.0"}]}"#).unwrap();
		assert_eq!(with_total.total, Some(12));
		let without_total: PointsPage = serde_json::from_str(r#"{"aspect":"p","time_unit":"millis","count":2,"offset":0,"points":[{"timestamp":1,"value":"1.0"}]}"#).unwrap();
		assert_eq!(without_total.total, None);
	}

	#[test]
	fn requests_serialize_as_the_server_reads_them() {
		let t = DateTime::parse_from_rfc3339("1970-01-01T00:00:00Z").unwrap().with_timezone(&Utc);
		let body = serde_json::to_value(Interpolate::new([(t, 1.0)]).spline(Spline::Polynomial { degree: 3 }).resolution("minutes")).unwrap();
		assert_eq!(body["spline"], serde_json::json!({"polynomial": {"degree": 3}}));
		assert_eq!(body["resolution"], "minutes");
		assert_eq!(body["points"][0]["timestamp"], "1970-01-01T00:00:00Z");
		assert!(body.get("start").is_none());
		assert_eq!(serde_json::to_value(Spline::Linear).unwrap(), "linear");
		let declared = serde_json::to_value(AspectDeclaration::new("p", "scaled_i64", "millis").scale(2).value_tolerance("0").unwrap()).unwrap();
		assert_eq!(declared, serde_json::json!({"name": "p", "physical_type": "scaled_i64", "timestamp_unit": "millis", "scale": 2, "value_tolerance": "0"}));
		let kind: PointKind = serde_json::from_str("\"someday\"").unwrap();
		assert_eq!(kind, PointKind::Unknown, "a new kind from a newer server does not fail the whole answer");
	}
}
