//! `InfluxDB` Line Protocol (ILP) parsing for dataset ingest.
//!
//! The parser now lives in the shared, vendor-neutral [`weft_line_protocol`]
//! crate so the benchmark harness and the HTTP server (`weft-server`) speak the
//! exact same ILP dialect. This module re-exports it unchanged, keeping the
//! historical `weft_bench::line_protocol::*` paths stable.

pub use weft_line_protocol::{parse, parse_points, FieldValue, LineRecord, ParseError, TimestampPrecision};
