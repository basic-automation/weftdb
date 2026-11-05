//! An async Rust client for `weft-server`'s HTTP API.
//!
//! One [`Client`] per server; each method is one request to one route (`docs/reference.md`,
//! "HTTP API"), named for what it does. Refusals come back as an [`Error`] carrying the
//! server's whole error envelope — `code` to branch on, `details`, `limit`, `committed`,
//! and the `request_id` an operator needs to find the request in a log.
//!
//! ```no_run
//! use weft_client::{AspectDeclaration, Client, WriteOptions};
//!
//! # async fn demo() -> weft_client::Result<()> {
//! let client = Client::builder("http://127.0.0.1:8080").api_key("weft_…").build()?;
//! client.declare_aspect(&AspectDeclaration::new("price", "scaled_i64", "millis").scale(2)).await?;
//! client.write_points("price", [(1_000, "1.25"), (2_000, "2.50")], &WriteOptions::idempotent("batch-1")).await?;
//! let page = client.points("price", 0, 9_999, &Default::default()).await?;
//! assert_eq!(page.points[0].value.as_ref().unwrap().to_string(), "1.25");
//! # Ok(()) }
//! ```
//!
//! Stored values are lossless: they are written from any [`StoredValue`] (decimal text,
//! [`BigDecimal`], integers) and read back as `BigDecimal`. An
//! `f64` is not a `StoredValue`, so a float cannot be written to the store by accident.
//! The compute endpoints are `f64`, as the server's are.
//!
//! Features: `rustls` for HTTPS (ring provider, platform or caller-supplied roots), and
//! `arrow` to decode Arrow IPC and Parquet answers (`decode_arrow` and
//! `decode_parquet`, both behind the `arrow` feature).
#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

#![warn(missing_docs)]

mod client;
mod error;
mod types;

pub use bigdecimal::BigDecimal;
pub use client::{Answer, Client, ClientBuilder};
pub use error::{Error, ErrorKind, Result};
pub use reqwest::Method;
pub use types::{parse_decimal, AspectDeclaration, AspectInfo, AspectStats, BenchRun, Bucket, ComputePoint, Downsample, Downsampled, Format, IlpOptions, Interpolate, Interpolated, InterpolatedPoint, OutputPoint, Page, PointAt, PointKind, PointsPage, Spline, StoreStats, StoredDownsample, StoredPoint, StoredValue, WriteOptions, WriteSummary};

/// The record batches of an Arrow IPC stream answer.
///
/// # Errors
///
/// [`ErrorKind::Decode`] if the bytes are not an Arrow IPC stream.
#[cfg(feature = "arrow")]
pub fn decode_arrow(bytes: &[u8]) -> Result<Vec<arrow_array::RecordBatch>> {
	let decode = |error: arrow_schema::ArrowError| Error::new(ErrorKind::Decode, format!("not an Arrow IPC stream: {error}")).with_source(error);
	let reader = arrow_ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None).map_err(decode)?;
	reader.collect::<std::result::Result<Vec<_>, _>>().map_err(decode)
}

/// The record batches of a Parquet answer.
///
/// # Errors
///
/// [`ErrorKind::Decode`] if the bytes are not a Parquet file.
#[cfg(feature = "arrow")]
pub fn decode_parquet(bytes: &[u8]) -> Result<Vec<arrow_array::RecordBatch>> {
	use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

	let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes)).and_then(ParquetRecordBatchReaderBuilder::build).map_err(|error| Error::new(ErrorKind::Decode, format!("not a Parquet file: {error}")).with_source(error))?;
	reader.collect::<std::result::Result<Vec<_>, _>>().map_err(|error| Error::new(ErrorKind::Decode, format!("not a Parquet file: {error}")).with_source(error))
}
