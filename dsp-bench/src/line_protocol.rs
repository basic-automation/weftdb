//! `InfluxDB` Line Protocol (ILP) parsing for dataset ingest.
//!
//! The parser now lives in the shared, vendor-neutral [`dsp_line_protocol`]
//! crate so the benchmark harness and the HTTP server (`dsp-server`) speak the
//! exact same ILP dialect. This module re-exports it unchanged, keeping the
//! historical `dsp_bench::line_protocol::*` paths stable.

pub use dsp_line_protocol::{parse, parse_points, FieldValue, LineRecord, ParseError, TimestampPrecision};
